# Spec: e2e-sandbox-gaps

## Problem Statement

The Docker E2E harness (`docker/Dockerfile.func`, `docker/Dockerfile.visual`,
the `just docker-*` / `just e2e-*` recipes, and the `scribe-test` driver) is
now the mandated sandbox for ALL scribe validation — CLAUDE.md/AGENTS.md carry
a hard rule forbidding any host-side control of scribe/scribe-dev because it
disrupts the developer's live sessions. A capability audit found 7 gaps that
prevent the harness from being the sole validation vehicle:

1. **Release-only builds** — both images `COPY target/release/*` only; no way
   to sandbox a debug or instrumented binary.
2. **No AI-tab launch / shell-env coverage** — the entire spec-018 surface
   (login shell env, `ai_tab_cwd`, restore-delta staging, argv construction)
   has zero automated scripts, and zsh/fish/nushell/PowerShell are not
   installed in either image.
3. **No `scribe-cli`** — neither image copies the `scribe`/`scribe-dev` CLI
   entry point, so CLI behavior cannot be validated in the sandbox.
4. **No local-IPC version-mismatch rig** — DESCOPED at clarify: local IPC has
   no version negotiation to test (see Clarifications Q1); remote-transport
   refusal coverage is the sanctioned check, and the limit is documented in
   US-7.
5. **No keyring/secret-service in the func image** — the visual image already
   ships a keyring fixture (`SCRIBE_KEYRING=1`), but the func image has no
   secret service, so the `.envz` envelope-persistence assertions are
   deliberately skipped (documented at `lat.md/test.md:203`).
6. **Coverage and ergonomics debt** — `just e2e` runs only 12 of 20 func
   scripts; there is no aggregate visual recipe (~40 visual scripts run one
   recipe at a time); `--gpus all` is hardcoded on every visual recipe even
   though rendering is software-only (lavapipe), so visual runs fail on hosts
   without the NVIDIA container toolkit; and no CI workflow runs any e2e, so
   harness rot is undetected.
7. **Inherent scope limits** — lavapipe software rendering only (no real GPU
   validation), Linux only (no macOS), and multi-client/mDNS/tailnet behavior
   only simulated via stand-ins (`share-tap`/`share-inject`, `lan-peer`,
   `remote-peer`). Confirmed at clarify as documented non-goals (US-7).

Solving these matters now because the sandbox-only rule was just made a hard
constraint: every capability the harness lacks is a validation task agents
must either skip or be tempted to run on the host.

## Goals

- A developer or agent can validate ANY scribe change class inside the
  sandbox, or the change class is named in US-7's change-class taxonomy with
  its sanctioned alternative recorded.
- Debug binaries can be built into and run in both images via a supported
  recipe parameter using a staging-dir strategy and separate `-debug` image
  tags (no Dockerfile editing).
- The spec-018 AI-tab launch/shell-env surface has automated func-level
  scripts covering bash, zsh, and fish; nushell/PowerShell are documented
  non-goals.
- `scribe-cli` is shipped in the func image and smoke-covered headless.
- Envelope persistence (`.envz`) is assertable in the func image by porting
  the visual image's `start_session_keyring` fixture.
- `just e2e` runs ALL func scripts (all 8 currently omitted scripts
  included); `just e2e-all-visual` runs the full visual suite by delegating
  to the existing per-script recipes with failure collection; GPU flags are
  opt-in, not hardcoded; CI runs a blocking smoke job on PRs and an
  informational nightly full suite so the harness cannot rot silently.
- The `/tests` mount becomes read-only and scripts gain an in-container
  sentinel guard, closing the two accidental host-touch paths.
- Every accepted limit that remains (gap 7 residue, local version pairing,
  nushell/PowerShell) is written into `lat.md/test.md` as an explicit
  non-goal with its alternative validation path.

## Non-Goals

Confirmed at the clarify gate:

- Real-GPU rendering validation, macOS coverage, and genuinely distributed
  multi-machine tests (real mDNS, real tailnet, two physical hosts) — gap
  7's inherent limits are DOCUMENTED as non-goals in US-7, not built.
- A local-IPC version-mismatch rig (former US-4) — local IPC has no version
  gate to exercise; old-server-over-local-IPC is out of contract
  (`specs/018-ai-tab-shell-env/verification.md:75`). Remote-transport
  refusal (`scribe-test remote-peer --refuse incompatible_version`) remains
  the sanctioned protocol-version check; documented in US-7.
- nushell and PowerShell shell coverage — not apt-installable on the trixie
  base (pinned tarball fetches, no pwsh arm64 story); documented in US-7.
- Instrumented builds (coverage/sanitizer profiles) — US-1 is debug-profile
  only.
- Hardening the sandbox profile itself (`--network none`, read-only rootfs,
  cap-drop from spec-018's ad-hoc profile) — a follow-up bead filed at
  create-beads, not part of this epic. (The narrow `/tests:ro` mount change
  and script sentinel guard ARE in scope — see US-6.)
- A socket-path override in `scribe-common` for isolated host runs — stays
  rejected; the container boundary remains the isolation mechanism and the
  CLAUDE.md rule the guard.
- Package/dpkg upgrade-path testing — covered by the separate
  `tests/install/` rig.
- New test frameworks or rewriting existing passing scripts. Existing
  per-script visual recipes stay the source of truth (US-6 delegates to
  them).

## Backlog Inputs

None — no `source_backlog` or epic-closure P4 sources were provided for this
run.

## Target Epic

No existing epic was provided or inferred; this run will create a new feature
epic at `create-beads`.

## User Stories

### US-1: Debug-build sandbox

As a scribe developer, I want to build the harness images from a debug build
profile, so that I can reproduce and debug failures with symbols, `dbg!`
output, and `RUST_LOG=trace` inside the sandbox instead of being tempted to
run a debug binary on the host.

**Acceptance criteria:**
- A recipe parameter (e.g. `just docker-func profile=debug`) selects the
  build profile; default remains release. Binaries are copied via a
  staging-dir strategy (the `.dockerignore` whitelist covers only
  `target/release/scribe-*`, so the recipe stages the selected profile's
  binaries into a context-visible dir before `docker build`).
- Debug builds produce separately tagged images
  (`scribe-test-func-debug`, `scribe-test-visual-debug`); release tags are
  never overwritten by a debug build.
- A named func smoke script (`tests/e2e/func/smoke.sh`) and a named visual
  smoke script pass against the debug images, with timeout budgets restated
  for debug speed (debug `scribe-client` is ~701 MB and slower under
  lavapipe).
- The recipe fails with a clear error when the selected profile's binaries
  are missing from the staging source; staleness = binary absent or older
  than the newest commit touching `crates/` (documented in the recipe).
- A documented "rerun one failing script against the debug image with
  RUST_LOG=trace" flow exists so agents can self-serve diagnosis.

### US-2: AI-tab launch and shell-env coverage

As a scribe maintainer, I want automated e2e scripts for the AI-tab launch
and shell-env behavior across bash, zsh, and fish, so that the spec-018
surface stops depending on manual, partly NOT-RUN verification.

**Acceptance criteria:**
- zsh and fish are installed in the func image (apt, no network fetches
  beyond apt).
- `scribe-test` gains the plumbing to request an AI-tab launch (flags on
  `session create` or a sibling subcommand plumbed through the daemon's
  `CreateSession`, which currently hardcodes `ai_launch: None`), plus a stub
  `claude` shim on PATH for argv assertions.
- Per-shell selection uses the passwd-first resolution path
  (`crates/scribe-common/src/shell.rs` `ai_login_shell`): scripts mutate the
  container's `/etc/passwd` entry (or use per-shell users) — mechanism
  chosen at planning, exercised per shell.
- Func scripts assert, for each of bash/zsh/fish: login-shell env
  construction, `ai_tab_cwd` (pane/project_root/home) resolution,
  restore-delta staging, and argv construction, per
  `specs/018-ai-tab-shell-env/spec.md`.
- The keyring-dependent NOT-RUN rows in
  `specs/018-ai-tab-shell-env/verification.md` are covered once US-5's
  fixture exists (explicit bead dependency); rows outside the bash/zsh/fish
  matrix are marked out of scope in that verification file.
- New scripts are wired into `just e2e` (US-6) and documented in
  `lat.md/test.md`.

### US-3: scribe-cli in the sandbox

As a scribe developer, I want the `scribe` CLI binary inside the func image,
so that CLI behavior is validatable in the sandbox.

**Acceptance criteria:**
- `scribe-cli` is built and copied into the func image alongside the other
  binaries (visual image only if a later story needs it).
- A func smoke script asserts headless-observable CLI surface: `scribe
  action ...` effects via the scribe-test daemon's `RunAction` path,
  `windows`/`profile` subcommand output, and server-absent error/exit-code
  behavior. (Bare `scribe` is an interactive socket passthrough —
  `crates/scribe-cli/src/main.rs:337` — not a client launcher, so no display
  is needed.)
- `lat.md/test.md` documents what CLI surface is and is not covered.

### US-4: Local-IPC version-mismatch rig — DESCOPED

Descoped at the clarify gate (Q1, option A). Local IPC has no version
negotiation (`ClientMessage::Hello` carries no version field); the only
version gate is `REMOTE_PROTOCOL_VERSION` on remote transports, already
covered by `scribe-test remote-peer --refuse incompatible_version`. Building
a local rig would require first implementing a local version gate — a
protocol feature, not harness work. The limit and its sanctioned alternative
are documented in US-7's taxonomy.

### US-5: Envelope persistence with a secret service

As a scribe maintainer, I want the visual image's keyring fixture ported to
the func image, so that `.envz` encrypted-envelope persistence is asserted
instead of skipped.

**Acceptance criteria:**
- `dbus` and `gnome-keyring` packages are added to `Dockerfile.func`, and
  `entrypoint-func.sh` gains a port of `entrypoint-visual.sh`'s
  `start_session_keyring` (dbus-launch + `gnome-keyring-daemon --unlock`)
  gated behind `SCRIBE_KEYRING=1`, started BEFORE `scribe-test server
  start` so the server sees `DBUS_SESSION_BUS_ADDRESS`.
- `entrypoint-func.sh`'s hardcoded `timeout 30` is replaced with a
  `TEST_TIMEOUT` override (default 30) mirroring the visual entrypoint.
- `tests/e2e/func/env-persistence.sh` (or a sibling) asserts the `.envz`
  write/read path end to end against a defined trigger (the persist event
  chosen at planning from the server's envelope-persist path); the
  documented skip at `lat.md/test.md:203` is removed or narrowed to state
  exactly what residual remains.
- The fixture is opt-in per script via the `SCRIBE_KEYRING` env flag;
  existing scripts keep their current behavior. No real secrets ever enter
  images or CI env (dummy unlock password only).

### US-6: Complete recipes, portable flags, host-safety, CI

As any contributor, I want `just e2e` to run everything func, an aggregate
visual recipe, GPU flags that don't break non-NVIDIA hosts, hardened
mounts, and CI, so that the harness is complete, portable, and rot-proof.

**Acceptance criteria:**
- `just e2e` runs ALL func scripts — the 8 currently omitted
  (`env-persistence`, `fresh-create-geometry`, `handoff-truecolor`,
  `keybindings-validation`, `multi-window`, `resize-coalescing`,
  `terminal-shortcuts`, `viewport-debounce`) are included; none excluded.
  (`env-persistence`'s keyring assertions depend on US-5 — bead ordering:
  US-5 before this recipe change lands.)
- `just e2e-all-visual` runs the full visual suite by DELEGATING to the
  existing per-script recipes (they remain the source of truth for
  per-script env), continues past failures, exits non-zero if any script
  failed, and writes a machine-readable per-script pass/fail summary under
  `test-output/`.
- `--gpus all` is removed from default recipe invocations; GPU passthrough
  becomes opt-in (e.g. `SCRIBE_E2E_GPUS=all`). Default visual runs succeed
  on hosts without the NVIDIA container toolkit — verified by the CI runner
  (which has no GPU) passing a visual smoke script.
- The `./tests/e2e:/tests` bind mount becomes read-only (`:ro`); `/output`
  is the only writable mount. Scripts gain a sentinel guard (abort with a
  loud error unless an in-container marker env/file is present) so running
  one directly on the host is impossible.
- CI: a blocking smoke job on PRs (cached release build + image build + a
  named small func/visual subset, with a stated wall-clock budget chosen at
  planning) and an informational nightly full-suite job that uploads
  `/output` artifacts (logs, screenshots, summary) on failure. Flake
  handling: nightly failures never block merges; a repeatedly-flaky script
  gets a quarantine bead, not a silent retry loop.
- Single-host concurrency limits (shared `test-output/`, single image tags)
  are documented in `lat.md/test.md` — parallel runs on one host are
  declared unsupported for now.

### US-7: Documented residual limits and change-class taxonomy

As a future agent bound by the sandbox-only rule, I want every remaining
harness limit written down with its sanctioned alternative, so that I never
have to guess whether host fallback is acceptable (it never is).

**Acceptance criteria:**
- `lat.md/test.md` gains a "Sandbox limits" section enumerating each
  confirmed non-goal — real GPU, macOS, real multi-machine, local-IPC
  version pairing (remote refusal is the sanctioned check),
  nushell/PowerShell shells, instrumented builds, parallel same-host runs —
  each with its sanctioned alternative (ask the user, perf A/B rig,
  `tests/install/` rig, remote-peer refusal, etc.). Verify the perf A/B rig
  reference is accurate before citing it.
- The same section carries a change-class taxonomy: server, client
  rendering, CLI, protocol, persistence, packaging, AI launch,
  sharing/remote — each mapped to its harness path or documented non-goal,
  making Goal 1 auditable.
- CLAUDE.md/AGENTS.md's sandbox rule cross-references that section. One
  line states that release workflows intentionally do not consume the e2e
  images.
- `lat check` passes.

## Constraints

- **Sandbox-only invariant:** all validation of these changes must itself run
  through the harness; never touch the host server
  (`crates/scribe-common/src/socket.rs:11-19` has no path override — the
  container boundary is the only isolation). Never restart the live server.
- **Evidence base:** `docker/Dockerfile.func:18-25` and
  `docker/Dockerfile.visual:70-76` (release-only COPY); `justfile:122-335`
  (recipes; `just e2e` subset at `justfile:324-335`; `--gpus all` at
  `justfile:138`); `crates/scribe-test/src/main.rs` (30+ subcommands);
  `specs/018-ai-tab-shell-env/verification.md:45-102` (NOT-RUN rows);
  `.dockerignore` (release-binary whitelist).
- **Constitution:** P3 (explicit, risk-based verification — this feature IS
  verification infrastructure; each story names its verifying script or
  check); P4 (perf budgets — planning must state numbers for func-image
  growth from shells+keyring, debug-image size, aggregate-suite wall-clock,
  and the PR smoke budget, or declare a given axis inapplicable explicitly);
  P5/P6 (no secrets in images or CI; container is the trust boundary; the
  func image is a test fixture, never a deployment artifact); P7
  (operationally safe change — never disrupt the live server; keep `lat.md`
  in sync; document compatibility decisions).
- CI runners must not require GPUs; CI jobs must use cargo/docker caching to
  keep the cold 40–90 min release build off the PR critical path.
- Keep additions to the func image; the visual image gains nothing new
  except the debug-tag variant.
- Nothing in the harness may depend on runtime network access (the future
  hardened-profile bead will set `--network none`).
- `just e2e`'s pre-US-6 scripts must stay green at every intermediate commit;
  the expanded definition applies once US-6 lands.
- Cross-story ordering (becomes bead dependencies): shared
  Dockerfile/justfile plumbing (US-1's staging mechanism) first; US-5 →
  US-2 keyring rows; US-5 → US-6 all-func recipe.

## Open Questions

All seven original open questions were resolved at the clarify gate — see
Clarifications. Remaining planning-level decisions (not blockers): exact
passwd-mutation mechanism for US-2 per-shell tests; the `.envz` persist
trigger for US-5's oracle; the named PR-smoke subset and its wall-clock
budget for US-6.

## Clarifications

Answers given at the human clarify gate (2026-08-02).

**Q1: US-4 disposition (local-IPC version-mismatch rig)?**
A: Option A — descope. Local version pairing becomes a documented US-7
non-goal; `scribe-test remote-peer --refuse incompatible_version` is the
sanctioned protocol-version check. US-4 rewritten as a descoped stub.

**Q2: Shell matrix for US-2?**
A: Option A — bash + zsh + fish (apt-installable). nushell/PowerShell are
documented non-goals in US-7. The required scribe-test AI-launch plumbing is
confirmed in scope.

**Q3: Sandbox contract items?**
A: All four approved — (A) gap-7 residue = documented non-goals; (B)
hardened docker profile = separate follow-up bead filed at create-beads;
(C) `/tests` mount goes read-only + scripts get an in-container sentinel
guard (US-6); (D) change-class taxonomy is a US-7 deliverable.

**Q4: CI shape?**
A: Option A — blocking smoke job on PRs (cached) + informational nightly
full suite with failure-artifact upload; flaky scripts get quarantine
beads.

**Q5: Debug-image mechanics?**
A: Option A — staging-dir strategy, separate `-debug` image tags, debug
profile only (instrumented/coverage out of scope), staleness = missing
binary or binary older than newest `crates/`-touching commit.

**Q6: Aggregate visual recipe?**
A: Option A — `e2e-all-visual` delegates to existing per-script recipes
(source of truth), collects failures, non-zero exit + machine-readable
summary; all 8 omitted func scripts join `just e2e`, none excluded.

**Q7: Keyring fixture?**
A: Option A — port visual's `start_session_keyring` to `entrypoint-func.sh`
behind `SCRIBE_KEYRING=1`, and replace the func entrypoint's hardcoded
`timeout 30` with a `TEST_TIMEOUT` override.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders). Feasibility review was code-grounded and found three
factual corrections to the draft (now folded into the body above):

- **US-4 premise was wrong.** Local IPC has no version negotiation:
  `ClientMessage::Hello` (`crates/scribe-common/src/protocol.rs:339`)
  carries no version field; `REMOTE_PROTOCOL_VERSION = 4` (protocol.rs:30)
  gates only the remote transports, where `scribe-test remote-peer
  --refuse incompatible_version` already covers refusal.
  `specs/018-ai-tab-shell-env/verification.md:75` declares
  old-server-over-local-IPC "OUT OF CONTRACT". → US-4 descoped (Q1).
- **US-5's gap claim was false for the visual image.** `gnome-keyring` is
  installed (`docker/Dockerfile.visual:53`) and
  `entrypoint-visual.sh:113,205-207` already starts a session keyring
  behind `SCRIBE_KEYRING=1`. → US-5 rewritten as a port (Q7).
- **US-3's stated concern was wrong.** Bare `scribe` is an interactive
  socket passthrough (`crates/scribe-cli/src/main.rs:337`), not a client
  launcher; `scribe action`/`windows`/`profile` are observable headless.
  → US-3 pinned func-side.

### Critical Questions (answered — see Clarifications)

1. US-4 disposition → descoped (Q1).
2. Shell matrix + scribe-test plumbing → bash/zsh/fish, plumbing in scope
   (Q2).
3. Sandbox contract: gap-7 non-goals, hardened-profile follow-up bead,
   `/tests:ro` + sentinel guard, change-class taxonomy → all approved (Q3).
4. CI shape and budget → smoke-on-PR + nightly full (Q4).
5. Debug-image mechanics → staging dir, `-debug` tags, debug-only (Q5).
6. Aggregate-recipe mechanics and recipe fate → delegate to existing
   recipes; all 8 func scripts included (Q6).
7. Keyring fixture shape → port `start_session_keyring` + `TEST_TIMEOUT`
   override (Q7).

### Non-Blocking Observations

- Perf/size budgets per P4: planning must attach numbers (func-image
  growth, debug-image size, suite wall-clock, PR smoke budget) or declare
  an axis inapplicable — now a Constraint.
- MVP ordering for partial delivery: GPU-flag fix (active blocker,
  mechanical) → US-1 → US-2 → US-7; US-3/US-5 next. Foundational
  Dockerfile/justfile plumbing should be one bead to avoid parallel-bead
  conflicts — now a Constraint.
- Cross-story ordering became explicit Constraints (US-5 → US-2 keyring
  rows → US-6 all-func).
- "Smoke" must be enumerated per use at planning (named scripts).
- Update `specs/018-ai-tab-shell-env/verification.md` NOT-RUN rows when
  US-2 lands; sweep `lat.md` refs (`lat refs`) for recipe mentions.
- No runtime-network dependencies; no secrets in images or CI — now
  Constraints.
- Release workflows intentionally do not consume these images — now a US-7
  doc line.
- Multi-arch: images are host-arch; arm64 consequences of the shell-matrix
  choice are moot for bash/zsh/fish (all apt-installable on arm64).
- Debug runs may need per-script `TEST_TIMEOUT` adjustments — folded into
  US-1 ACs; diagnosability flow folded into US-1.
