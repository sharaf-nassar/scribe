# Plan: e2e-sandbox-gaps

## Architecture Approach

The harness is extended in place — no new frameworks, no image redesign. Four
mechanisms carry the whole feature:

1. **Profile-parameterized image builds via a staging directory.** The
   `docker-func` / `docker-visual` recipes gain a `profile="release"`
   parameter. A new helper (`tools/e2e-stage.sh`) copies the selected
   profile's binaries from `target/<profile>/` into a context-visible staging
   dir (`target/e2e-stage/<profile>/`), failing loudly when a binary is
   missing or older than the newest commit touching `crates/`. The
   Dockerfiles take `ARG BIN_DIR=target/e2e-stage/release` and COPY from it;
   `.dockerignore` whitelists the staging dir. Debug builds produce separate
   `scribe-test-func-debug` / `scribe-test-visual-debug` tags so release tags
   are never overwritten (Clarification Q5). *Rejected alternative:*
   editing the Dockerfiles per run or a build-arg pointing straight at
   `target/debug/` — the `.dockerignore` whitelist (`.dockerignore:5-10`)
   only admits `target/release/scribe-*`, and widening it to all of
   `target/debug` would ship a multi-GB context; the staging dir keeps the
   context minimal and makes "which binaries went in" explicit.

2. **No local-IPC protocol change at all for AI-launch plumbing.**
   `ClientMessage::CreateSession` already carries
   `#[serde(default)] ai_launch: Option<AiLaunchSpec>`
   (`crates/scribe-common/src/protocol.rs:268-270`, spec-018), with msgpack
   round-trip proof tests (`protocol.rs:1447`, `:1489`). The gap is purely in
   `scribe-test`: its daemon hardcodes `ai_launch: None`
   (`crates/scribe-test/src/daemon.rs:994`) and the CLI exposes no flag. The
   plan adds flags to `scribe-test session create` and optional fields to the
   harness-internal `DaemonRequest::CreateSession`
   (`crates/scribe-test/src/cmd_socket.rs:24`) — a same-binary, same-version
   private protocol, so additive fields there are risk-free. A committed
   `tests/e2e/bin/claude` stub (new dir, put on PATH by the entrypoints)
   records argv + environment to a writable location for assertions.
   *Rejected alternative:* driving AI tabs through `scribe-cli action
   new-claude-tab` — that dispatches to a connected GUI window
   (`crates/scribe-cli/src/main.rs:268`), which the func harness doesn't
   have.

3. **Port, don't invent, the keyring fixture.** `entrypoint-visual.sh:113-122`
   (`start_session_keyring`: `dbus-launch` + `gnome-keyring-daemon --unlock
   --components=secrets`, dummy empty password) is copied into
   `entrypoint-func.sh` behind `SCRIBE_KEYRING=1`, started before
   `scribe-test server start` so the server inherits
   `DBUS_SESSION_BUS_ADDRESS` (Clarification Q7). This unblocks the sole
   production `.envz` writer (`crates/scribe-server/src/env_store/store.rs:129`,
   reached only via the debounced `schedule_persist` at
   `hook_ingress.rs:303`).

4. **Aggregation by delegation.** `just e2e` grows to list every func script;
   `just e2e-all-visual` is a loop that shells out to the existing
   per-script recipes (which remain the per-script env source of truth,
   Clarification Q6), continues past failures, writes a machine-readable
   summary under `test-output/`, and exits non-zero on any failure. GPU
   passthrough becomes a `SCRIBE_E2E_GPUS` opt-in evaluated by a justfile
   variable, removing the hardcoded `--gpus all` from all ~30 visual recipes
   (`justfile:138-320`). CI (Clarification Q4) is a new workflow: a blocking
   cached PR smoke plus an informational nightly full suite with artifact
   upload.

Constitution fit: P1 — all changes stay inside existing crate/file
responsibilities; the frozen local IPC protocol is untouched. P3 — this
feature *is* verification infrastructure and test code is explicitly
requested by the spec; every work item below names its verifying script.
P5/P6 — the container remains the trust boundary; the only "secret" anywhere
is the keyring's dummy empty unlock password; nothing in the harness gains a
runtime network dependency. P7 — all validation runs through the harness
itself; the live host server is never touched or restarted; `lat.md` updates
ride inside each work item. Tension noted: P3 prefers blocking verification,
but the nightly full suite is deliberately informational (Q4) to keep flake
from blocking merges — the blocking guarantee lives in the PR smoke, and
repeat flakes get quarantine beads instead of retries.

## Affected Components

- `justfile` — `docker-func` / `docker-visual` (`justfile:125-130`) gain
  `profile` param + staging call + conditional `-debug` tag; `e2e-func` /
  `e2e-visual` / `e2e-visual-shared` (`justfile:133-147`) gain an optional
  `image` param and `-e TEST_TIMEOUT -e RUST_LOG -e SCRIBE_KEYRING`
  passthrough; every visual recipe (`justfile:138-320`) loses hardcoded
  `--gpus all` in favor of a `gpu_flags` variable from `SCRIBE_E2E_GPUS`;
  all `-v ./tests/e2e:/tests` mounts become `:ro`; `e2e` (`justfile:323-335`)
  grows from 12 to all 20 existing func scripts plus the new US-2/US-3
  scripts; new recipe `e2e-all-visual`. No dedicated debug-wrapper
  recipes: the `image=` parameter on `e2e-func` / `e2e-visual` already
  covers the diagnosis flow.
- `docker/Dockerfile.func` — `ARG BIN_DIR`; COPY lines (`:18-25`) switch to
  `${BIN_DIR}`; add `COPY ${BIN_DIR}/scribe-cli` (US-3); apt adds `zsh`,
  `fish` (US-2), `dbus`, `dbus-x11`, `gnome-keyring` (US-5; `dbus-launch`
  ships in `dbus-x11` on trixie, mirroring `Dockerfile.visual:52-53`).
- `docker/Dockerfile.visual` — `ARG BIN_DIR` on COPY lines (`:70-76`) only;
  no new packages (spec constraint: visual image gains nothing but the
  debug-tag variant).
- `.dockerignore` — whitelist `target/e2e-stage/` alongside the existing
  release-binary whitelist (`:4-10`).
- `tools/e2e-stage.sh` — new: stage binaries per profile, missing/stale
  checks.
- `docker/entrypoint-func.sh` — port `start_session_keyring` (from
  `entrypoint-visual.sh:113-122`) behind `SCRIBE_KEYRING=1` before
  `scribe-test server start` (`:17`); replace `timeout 30` (`:24`) with
  `timeout "${TEST_TIMEOUT:-30}"`; add `export PATH="/tests/bin:$PATH"`
  (visual already has it at `entrypoint-visual.sh:235`); export the sentinel
  `SCRIBE_E2E_SANDBOX=1`.
- `docker/entrypoint-visual.sh` — export the sentinel `SCRIBE_E2E_SANDBOX=1`.
- `crates/scribe-test/src/main.rs` — `SessionAction::Create` (`:317`) gains
  `--ai-provider`, `--ai-resume-mode`, `--ai-conversation-id`, `--cwd`,
  `--env-envelope-id` flags.
- `crates/scribe-test/src/cmd_socket.rs` — `DaemonRequest::CreateSession`
  (`:24`) gains matching optional fields (harness-internal protocol).
- `crates/scribe-test/src/daemon.rs` — `handle_create_session` (`:962-997`)
  passes `ai_launch: Some(AiLaunchSpec{..})`, `cwd`, and an optional caller
  supplied envelope id instead of the hardcoded `None` / fresh
  `new_launch_id()`.
- `crates/scribe-test/src/session.rs` (`:21`) — thread the new fields.
- `tests/e2e/bin/claude` — new committed stub shim (argv/env dump to
  `${SCRIBE_AI_STUB_OUT:-/tmp}`; `/tests` is read-only so it must never
  write beside itself).
- `tests/e2e/func/` — new scripts: `ai-launch-smoke.sh`,
  `ai-shell-env-bash.sh`, `ai-shell-env-zsh.sh`, `ai-shell-env-fish.sh`,
  `cli-smoke.sh`; extended: `env-persistence.sh`; all 20+ existing func and
  ~41 visual scripts get the 3-line sentinel-guard prologue.
- `.github/workflows/e2e.yml` — new workflow (PR smoke + nightly full);
  `quality.yml` and `release.yml` are untouched (release intentionally does
  not consume e2e images — a US-7 doc line).
- `specs/018-ai-tab-shell-env/verification.md` — NOT-RUN rows at `:45`
  (keyring), `:47` and `:53` (zsh/fish) flip to covered; nushell/PowerShell/
  unknown-shell and known-old-server rows (`:53-54`, `:73`) marked out of
  scope with a pointer to the US-7 taxonomy.
- `lat.md/test.md` — per-item updates plus a new top-level `## Sandbox
  limits` section inserted after `## Visual E2E Tests` (~line 580); the
  `.envz` skip sentence at `:203` removed/narrowed; recipe docs (`just e2e`,
  `just docker-func`, `just e2e-all-visual`) added; single-host concurrency
  limits documented.
- `CLAUDE.md` / `AGENTS.md` — sandbox rule cross-references the new
  "Sandbox limits" section.

## Data Model

- **Local IPC protocol: none.** `ai_launch`, `cwd`, and `env_envelope_id`
  already exist as `#[serde(default)]` optional fields on
  `ClientMessage::CreateSession` (`protocol.rs:253-275`); the harness merely
  starts populating them. No `REMOTE_PROTOCOL_VERSION` bump.
- **scribe-test internal daemon protocol** (not frozen; same binary on both
  ends): `DaemonRequest::CreateSession` gains
  `ai_provider: Option<AiProvider>`, `ai_resume_mode: Option<AiResumeMode>`,
  `ai_conversation_id: Option<String>`, `cwd: Option<PathBuf>`,
  `env_envelope_id: Option<String>` — all defaulted so existing call sites
  compile unchanged.
- **Staging-dir layout:** `target/e2e-stage/<profile>/{scribe-server,
  scribe-client, scribe-test, scribe-hook-helper, scribe-cli}` — flat,
  profile-keyed, recreated on every `docker-*` recipe run, git-ignored.
- **Aggregate summary format:** `test-output/e2e-visual-summary.jsonl`, one
  object per script:
  `{"script": "visual/bell.sh", "recipe": "e2e-visual-bell",
  "status": "pass"|"fail", "exit_code": <int>, "duration_s": <int>}`,
  written append-per-script so a crashed run still leaves a partial record.
  `just e2e` failures need no new format (fail-fast recipe list, unchanged
  semantics).
- **Claude-stub record:** `${SCRIBE_AI_STUB_OUT:-/tmp}/claude-invocation.txt`
  — argv one-per-line, then `env` output; consumed only by the US-2 scripts.

## API / Interface Changes

- **`scribe-test session create`** (new flags, all optional; absent flags
  reproduce today's behavior exactly):
  `--ai-provider <claude|codex>` — populates `AiLaunchSpec.provider`;
  `--ai-resume-mode <new|resume>` (default `new` when a provider is given);
  `--ai-conversation-id <id>` — with `resume`, exercises the quoting path
  (`session_manager.rs:1057-1073`);
  `--cwd <path>` — populates `CreateSession.cwd`;
  `--env-envelope-id <id>` — overrides the daemon's minted `new_launch_id()`
  (`daemon.rs:988`) so an AI tab can be pointed at a pre-seeded envelope for
  restore-delta assertions.
- **Daemon request fields:** as in Data Model; `handle_create_session`
  builds `ai_launch: Some(..)` when a provider is present.
- **Just recipe signatures:**
  `docker-func profile="release"` / `docker-visual profile="release"` —
  `profile` accepts exactly `release|debug` and errors on any other value
  (Clarification Q5); `profile=debug` tags `scribe-test-{func,visual}-debug`;
  `e2e-func script image="scribe-test-func"` (and visual equivalents) — the
  debug rerun flow is `just e2e-func func/x.sh image=scribe-test-func-debug`
  with `TEST_TIMEOUT` / `RUST_LOG=trace` passed through;
  `e2e-all-visual` — no params, delegates to per-script recipes;
  `e2e` — unchanged signature, expanded body.
- **GPU opt-in:** justfile variable
  `gpu_flags := if env("SCRIBE_E2E_GPUS", "") == "" { "" } else { "--gpus "
  + env("SCRIBE_E2E_GPUS") }`; every visual recipe uses `{{gpu_flags}}`;
  default runs pass no `--gpus` (lavapipe is pinned anyway,
  `Dockerfile.visual:63`).
- **Entrypoint env contract additions:** func gains `SCRIBE_KEYRING=1`
  (opt-in session keyring, started before the server) and `TEST_TIMEOUT`
  (default 30, mirroring visual's default-60 contract at
  `entrypoint-visual.sh:6`); both entrypoints export
  `SCRIBE_E2E_SANDBOX=1`.
- **Sentinel-guard contract for scripts:** every script under
  `tests/e2e/{func,visual}/` begins (after the shebang/comment block) with:
  `[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only
  runs inside the scribe e2e container (use just e2e-func / e2e-visual)."
  >&2; exit 99; }` — host invocation aborts before any command runs.
- **NO breaking local-IPC changes:** the frozen protocol is not modified at
  all; the fields the harness starts sending are the existing additive
  `#[serde(default)]` msgpack-named fields proven by
  `create_session_missing_ai_launch_defaults_to_none` (`protocol.rs:1489`).
- **Mount contract:** `/tests` becomes `:ro` in every recipe; `/output`
  stays the only writable mount (the claude stub and scripts write only to
  `/output` or `/tmp`).

## Testing Strategy

The harness validates itself: every work item lands with its own script run
through the docker recipes, and nothing ever touches the host server
(`socket.rs:11-19` has no override; the container boundary is the isolation
— P7).

Per user story:

- **US-1:** `just docker-func profile=debug && just e2e-func func/smoke.sh
  image=scribe-test-func-debug` (with `TEST_TIMEOUT=90`) and
  `just docker-visual profile=debug && just e2e-visual visual/titlebar.sh
  image=scribe-test-visual-debug` (with `TEST_TIMEOUT=240`) both pass;
  deleting a staged binary makes the recipe fail with the documented error;
  tag isolation is proven by capturing `docker images -q
  scribe-test-func:latest`, running `just docker-func profile=debug`, and
  asserting the release tag's image ID is unchanged.
- **US-2:** `func/ai-launch-smoke.sh` proves the plumbing (stub argv
  observed); `func/ai-shell-env-{bash,zsh,fish}.sh` each assert: argv shape
  `<shell> -lic 'exec claude ...'` via the stub dump (per
  `session_manager.rs:1023-1054`); login-file sourcing order via marker
  lines planted in `~/.bash_profile` / `~/.zprofile` / fish `config.fish`;
  `SCRIBE_AI_TAB=1`, `SCRIBE_INTEGRATION_SCRIPT` env-var (never
  interpolated) presence; cwd contract — `--cwd <real dir>` honored,
  `--cwd /nonexistent` falls back through the server guard
  (`session_manager.rs:793`) to `$HOME` (func scripts assert the server-side
  contract for the values a client in pane/project_root/home mode would
  send; client-side mode *selection* stays covered by the GPUI unit suites
  per spec-018). Restore-delta staging rows run only after US-5 (see
  Sequencing): seed an envelope in a plain session, re-create an AI tab with
  `--env-envelope-id`, assert the delta env survives into the stub dump and
  `SCRIBE_RESTORE_ENV_DELTA_FILE` is unset/deleted.
- **US-3:** `func/cli-smoke.sh` — `scribe profile list/save/switch`
  (serverful container but purely local file ops), `scribe windows` output
  shape, `scribe action --window <id> new-tab` observed via
  `scribe-test`-side session count; then `scribe-test server stop` and
  assert `scribe windows` exits 1 with an IO error (no-socket behavior,
  `scribe-cli/src/main.rs:161`).
- **US-5:** extended `func/env-persistence.sh` under `SCRIBE_KEYRING=1` —
  trigger is a shell-reported env delta (`export FOO=...` in the session
  shell with integration active → `scribe-hook-helper --event=env_delta` →
  `EnvChanged` → debounced `schedule_persist`, 100 ms debounce at
  `env_store/mod.rs:35`); the script polls for
  `<state_dir>/restore/env/<window_id>/<launch_id>.envz` (path format at
  `store.rs:135`, id from `scribe-test session envelope-id`) and asserts
  round-trip via a fresh session staging the delta. Without
  `SCRIBE_KEYRING`, the script's pre-existing degraded-path assertions keep
  passing (opt-in, no behavior change for other scripts).
- **US-6:** the expanded `just e2e` passing end-to-end is its own proof;
  `just e2e-all-visual` verified by injecting one known-failing script run
  and asserting continue-past-failure + non-zero exit + a complete summary
  JSONL; GPU opt-in verified on the GPU-less CI runner passing
  `visual/titlebar.sh`; `:ro` mount verified by a script attempting a write
  to `/tests` and asserting EROFS (one-off check, not a committed test);
  sentinel verified by invoking one script on the host and observing exit 99
  before any side effect.
- **US-7:** `lat check` passes (MCP `lat_check`); the taxonomy is reviewed
  against the Goals bullet ("any change class → harness path or documented
  non-goal"); the perf-rig citation is verified accurate:
  `tools/perf-ab-rig/run-perf-ab.sh` (`--ai-tab-only --live`), documented at
  `lat.md/test.md:1151` — it is a bare script, not a just recipe, and
  `--live` attaches to the already-running isolated `scribe-dev` server
  without restarting it.

**P4 budgets (numbers, or explicitly inapplicable):**

- Func image growth (zsh+fish+dbus+dbus-x11+gnome-keyring+scribe-cli):
  expected ~+60 MB; budget **≤ +150 MB** over the current 2.82 GB
  (`docker images` after `docker-func` is the measurement).
- Debug image size: measured debug binaries — server 248 MB, test 275 MB,
  hook-helper 45 MB, cli 76 MB, client 702 MB — give func-debug ~+0.6 GB
  (~3.45 GB) and visual-debug ~+1.2 GB (~4.9 GB, the known growth); budget
  **debug tag ≤ release tag + 1.5 GB**.
- PR smoke wall-clock: target **≤ 25 min** with warm rust-cache (release
  incremental build + 2 image builds + 4 scripts); hard job timeout 40 min;
  first cold-cache run is exempt (40–90 min cold build acknowledged in the
  spec constraints).
- Nightly full suite: expected **2–3 h** (≈24 func scripts at ≤ 2 min each
  plus ~30 visual recipe invocations at 60–300 s TEST_TIMEOUT plus container
  overhead); job timeout 4 h.
- Debug smoke timeouts: func smoke `TEST_TIMEOUT=90` (3× default), visual
  titlebar `TEST_TIMEOUT=240` (4× default) — restated in the recipes'
  comments per US-1.
- Runtime perf of scribe itself: **inapplicable** — this feature changes no
  hot path; the sanctioned perf tool remains the perf A/B rig, cited in
  US-7.

## Risks

- **Debug-binary size/slowness → timeout flakiness.** 702 MB debug client
  under lavapipe loads slowly; first-window waits may exceed the entrypoint
  helpers' 15–20 s internal timeouts, not just `TEST_TIMEOUT`. Mitigation:
  budgets above, `TEST_TIMEOUT` multipliers documented per debug smoke, and
  debug images kept out of CI entirely (release-only there). Rollback: the
  `-debug` tags are additive; deleting them restores status quo.
- **passwd-mutation mechanism for per-shell tests.** Decision: scripts run
  `usermod -s /usr/bin/zsh root` (container runs as root; libc re-reads
  `/etc/passwd` on each `getpwuid`, and `ai_login_shell()` at
  `shell.rs:56` is called per launch, so no server restart is needed).
  Risk: an nscd-style cache or a future non-root container breaks this.
  Mitigation: each script asserts the resolution tier by grepping the server
  log for `tier = "passwd"` (`session_manager.rs:875` logs
  `source.as_str()`); fallback mechanism (pre-created per-shell users +
  `setpriv`) documented in the script header but not built.
- **`.envz` persist-trigger uncertainty.** The only production trigger is
  the debounced env-delta path; there is no flush-on-shutdown
  (`env_store/mod.rs` has no such path), so a script that sets a var and
  immediately exits the shell can lose the pending debounce. Mitigation: the
  script keeps the session alive and polls for the file (≥ 100 ms debounce +
  margin); if the trigger proves unreliable, the fallback oracle is the
  `EnvStatusState::Active` transition in the server log plus
  `read_envelope` via a second session — documented in the bead.
- **gnome-keyring-as-root quirks in the func image.** `gnome-keyring-daemon`
  wants mlock (CAP_IPC_LOCK — root has it by default in docker) and a
  `/run/user/0` control dir; `dbus-launch` lives in `dbus-x11`, not `dbus`.
  Mitigation: port the visual fixture verbatim (it already works as root in
  the visual container per `lat.md/test.md:488`), install `dbus-x11`
  explicitly, and gate everything behind `SCRIBE_KEYRING=1` so failure is
  contained to opted-in scripts.
- **Aggregate recipe error-collection across ~30 recipes.** `just` inside a
  recipe loop swallows or aborts on child failures depending on shell flags.
  Mitigation: `e2e-all-visual` invokes each child as `just <recipe> ||
  status=$?` inside a `bash -u` loop with `set +e` semantics per iteration,
  appends the JSONL row immediately, and exits with the OR of statuses;
  verified by the injected-failure check in Testing Strategy.
- **CI caching effectiveness.** If `Swatinem/rust-cache` misses (lockfile
  churn), the PR smoke blows its budget. Mitigation: `prefix-key` shared
  with `release.yml`'s `v1-rust` where possible, budget has 15 min headroom,
  and the job timeout (40 min) converts a pathological miss into a visible
  failure rather than a silent hour-long queue. Docker layer caching is
  IN: `buildx` gha cache with named scopes (`e2e-func`, `e2e-visual`); the
  image build is only ~2–3 min on top of staged binaries, so it is not
  budget-critical.
- **Knowledge risk: wrong citation baked into spec 019.** The US-4 descope
  cites `specs/018-ai-tab-shell-env/verification.md:75` as declaring
  old-server-over-local-IPC "OUT OF CONTRACT", but that string exists
  nowhere in the file — `:73` is an availability-limitation row. The descope
  itself remains sound (no version field on `ClientMessage::Hello`,
  `protocol.rs:339`; `REMOTE_PROTOCOL_VERSION = 4` gates remote only). The
  US-7 work item records the correct citation instead of propagating the
  wrong one. The clarified descope decision itself is NOT reopened.
- **Sentinel sweep touches ~60 scripts.** Purely mechanical but wide; a
  missed script silently stays host-runnable. Mitigation: a one-line grep
  check (`grep -L SCRIBE_E2E_SANDBOX tests/e2e/*/*.sh`) run in the same work
  item and cited in its acceptance criteria (candidate for a later `ready`
  gate, not added now).
- **Rollback generally:** every mechanism is opt-in or additive (new flags,
  new tags, new recipes, env-gated fixtures); the two default-behavior
  changes (`:ro` mount, GPU-flag removal) are single-line justfile reverts.

## Sequencing

Work items below become the bead DAG; ordering is expressed as
"depends on" prose. Each includes its `lat.md` sync and names its verifying
check (P3, P7).

- **Image build plumbing: profile staging, debug tags, func-image
  additions.** One work item owns ALL Dockerfile/`.dockerignore`/
  `docker-*`-recipe changes to avoid parallel conflicts (spec constraint):
  `tools/e2e-stage.sh`, `profile` params (constrained to exactly
  `release|debug`, erroring on anything else per Clarification Q5),
  `-debug` tags, missing/stale binary errors, plus the func-image apt
  additions (zsh, fish, dbus, dbus-x11, gnome-keyring) and the
  `scribe-cli` COPY. The staleness rule is additionally stated as a
  comment inside the `docker-func` / `docker-visual` recipe bodies
  themselves. Depends on: nothing (foundational; lands first).
  Acceptance: `just docker-func && just e2e-func func/smoke.sh` green and
  release tags unchanged; `just docker-func profile=debug` yields
  `scribe-test-func-debug`; any other `profile` value errors; a
  deliberately removed staged binary produces the documented error; the
  STALE branch is exercised — backdate a staged binary's mtime (or land a
  trivial `crates/` commit without rebuilding) and assert the documented
  stale error; the staleness-rule comment is present in both the
  `docker-func` and `docker-visual` recipes; func image growth within the
  +150 MB budget; `lat.md/test.md` documents the profile parameter.

- **GPU opt-in, read-only /tests, sentinel guard.** Small early item and
  active blocker for non-NVIDIA hosts: `gpu_flags` variable replaces every
  hardcoded `--gpus all` — as the active-blocker fix, the `gpu_flags` hunk
  is front-loaded within this item's commit stream; all `/tests` mounts
  become `:ro`; both entrypoints export `SCRIBE_E2E_SANDBOX=1` (the
  `entrypoint-visual.sh` sentinel export is the sanctioned exception to
  the "visual image gains nothing new" constraint — no packages or
  binaries are added); all existing func+visual scripts get the guard
  prologue; `e2e-func`/`e2e-visual` gain the `image` param and env
  passthrough. Because the recipe passthrough lands here, this item
  explicitly owns ALL `entrypoint-func.sh` edits except the
  keyring-fixture port: the `timeout "${TEST_TIMEOUT:-30}"` change, the
  `SCRIBE_E2E_SANDBOX=1` sentinel export, and the
  `export PATH="/tests/bin:$PATH"` line all land in this item; the
  keyring item ports only `start_session_keyring`, and no other item
  touches `entrypoint-func.sh`. Rollback: the `:ro` and GPU-flag default
  changes are single-line justfile reverts. Depends on: the image-build
  plumbing item (serializes justfile ownership). Acceptance: `just
  e2e-func func/smoke.sh` and `just e2e-visual visual/titlebar.sh` pass
  with `SCRIBE_E2E_GPUS` unset on a non-NVIDIA host; host-side direct
  invocation of one script exits 99;
  `grep -L SCRIBE_E2E_SANDBOX tests/e2e/*/*.sh` is empty; the `:ro`
  contract is verified by a named one-off check (container-side
  `touch /tests/x` fails with EROFS) or a `grep -c ':ro'` count over the
  justfile matching the mount sites; a `lat refs`/grep sweep confirms no
  stale recipe mentions (old `--gpus` invocations) survive in docs;
  `lat.md/test.md` gains a named section (`## E2E Recipe Contract`)
  covering the `SCRIBE_E2E_GPUS` opt-in, `:ro` mounts, the sentinel, and
  the `image=` recipe parameter, and `lat check` passes.

- **Debug-image smoke validation and diagnosis flow.** Runs the named debug
  smokes (`func/smoke.sh`, `visual/titlebar.sh` against the `-debug` tags)
  with restated `TEST_TIMEOUT` budgets, and documents the "rerun one failing
  script against the debug image with RUST_LOG=trace" flow in
  `lat.md/test.md`. Depends on: image build plumbing; GPU/sentinel item
  (final recipe shape). Acceptance: both debug smokes green within budget;
  the documented rerun command works verbatim. Completes US-1.

- **scribe-test AI-launch plumbing and claude stub.** New `session create`
  flags, `DaemonRequest` fields, daemon construction of
  `AiLaunchSpec`/`cwd`/`--env-envelope-id` override, committed
  `tests/e2e/bin/claude` stub, plus a minimal `func/ai-launch-smoke.sh`
  proving the stub is exec'd with the expected argv. This item makes NO
  `entrypoint-func.sh` edits — the `PATH=/tests/bin` export it relies on
  lands in the GPU/sentinel item. Depends on: GPU/sentinel item (for the
  guard prologue contract in the new script and the `PATH=/tests/bin`
  export); can proceed in parallel with the debug-smoke item. Acceptance:
  `cargo test -p scribe-test` and `cargo test -p scribe-common` green;
  `just e2e-func func/ai-launch-smoke.sh` green; no change to
  `crates/scribe-common` protocol definitions (diff-verified);
  `lat.md/test.md` gains a named section (`## AI-Launch Harness
  Plumbing`) covering the new `session create` AI-launch flags and the
  claude-stub contract, and `lat check` passes.

- **AI-tab shell-env matrix scripts (bash/zsh/fish, non-keyring rows).**
  `func/ai-shell-env-{bash,zsh,fish}.sh` asserting argv, login-env/startup
  file order, integration env vars, and the cwd contract per shell, using
  `usermod -s` passwd mutation and the `tier = "passwd"` log oracle.
  Depends on: image build plumbing (shells present); AI-launch plumbing.
  Acceptance: all three scripts green via `just e2e-func`; server log shows
  passwd-tier resolution for each shell; `lat.md/test.md` gains their spec
  sections; the US-2 `ai_tab_cwd` narrowing is recorded — client-side mode
  *selection* stays covered by the spec-018 GPUI unit suites (no suite is
  cited by name in the spec-018 docs, so the record points at
  `specs/018-ai-tab-shell-env/verification.md`), and that narrowing is
  written into `specs/018-ai-tab-shell-env/verification.md` and
  `lat.md/test.md`. Covers the non-keyring half of US-2.

- **scribe-cli func smoke script.** `func/cli-smoke.sh` per the US-3
  strategy above, plus the `lat.md/test.md` statement of covered vs
  not-covered CLI surface (interactive passthrough and GUI-dispatch actions
  are observed only headlessly). Depends on: image build plumbing
  (binary present); GPU/sentinel item. Acceptance: script green via
  `just e2e-func func/cli-smoke.sh`. Completes US-3.

- **Keyring fixture port and .envz persistence assertion.** Port
  `start_session_keyring` into `entrypoint-func.sh` behind
  `SCRIBE_KEYRING=1` (before server start) — the ONLY
  `entrypoint-func.sh` edit this item makes (the `TEST_TIMEOUT` override,
  sentinel export, and PATH export already landed in the GPU/sentinel
  item), extend `func/env-persistence.sh` with the
  keyring-gated `.envz` write/read assertions against the debounced
  env-delta trigger, and narrow the skip note at `lat.md/test.md:203` to
  the exact residual (nothing, or the no-flush-on-shutdown caveat).
  Depends on: image build plumbing (packages present); GPU/sentinel item
  (owns the baseline `entrypoint-func.sh` edits).
  Acceptance: `SCRIBE_KEYRING=1 just e2e-func func/env-persistence.sh`
  asserts a real `.envz`; the same script without the flag still passes its
  degraded-path assertions. Completes US-5.

- **Keyring-dependent AI-launch rows and spec-018 verification sync.**
  Restore-delta staging assertions per shell (seed envelope → AI tab with
  `--env-envelope-id` → delta visible in stub dump, staging file consumed),
  then update `specs/018-ai-tab-shell-env/verification.md`: flip `:45`
  (envelope staging) and `:47`/`:53` zsh/fish rows to covered with script
  names; mark nushell/PowerShell/unknown-shell and known-old-server rows out
  of scope pointing at the US-7 taxonomy. Depends on: shell-env matrix item;
  keyring fixture item (this is the spec's US-5 → US-2 keyring-rows
  ordering). Mechanism, by name: keyring-gated blocks appended to the
  existing `func/ai-shell-env-{bash,zsh,fish}.sh` scripts, skipped unless
  `SCRIBE_KEYRING=1`. Acceptance: extended shell scripts green under the
  exact rerun command
  `SCRIBE_KEYRING=1 just e2e-func func/ai-shell-env-bash.sh` (and its zsh/
  fish equivalents); verification.md rows updated. Completes US-2.

- **Full func suite and aggregate visual recipe.** `just e2e` lists all 20
  existing func scripts — the 8 previously omitted ones being
  `env-persistence`, `fresh-create-geometry`, `handoff-truecolor`,
  `keybindings-validation`, `multi-window`, `resize-coalescing`,
  `terminal-shortcuts`, and `viewport-debounce` — plus `ai-launch-smoke`,
  the three shell-env scripts,
  and `cli-smoke` (env-persistence runs with `SCRIBE_KEYRING=1`); new
  `just e2e-all-visual` delegates to the existing per-script recipes (and
  plain `e2e-visual <script>` for scripts without one), continues past
  failures, writes the JSONL summary, exits non-zero on any failure;
  document single-host concurrency limits (shared `test-output/`, single
  tags, single `/run/user/<uid>` socket namespace per
  `socket.rs:18-19`) in `lat.md/test.md`. Depends on: keyring fixture item
  (spec's US-5 → US-6 all-func ordering); shell-env matrix item; cli smoke
  item; GPU/sentinel item. Rollback: the quarantine-bead policy extends
  to the local `e2e` list — a flaky script may be temporarily dropped
  from the recipe with a bead filed. Acceptance: `just e2e` fully green;
  a diff of `ls tests/e2e/func/*.sh` against the `e2e` recipe body is
  clean, so a silently-excluded script fails the check; injected
  failure proves the aggregate's collection semantics; summary file schema
  matches Data Model; a `lat refs`/grep sweep confirms no stale mentions
  of the pre-US-6 `e2e` list survive in docs. Covers the recipe half
  of US-6. Interim commits keep
  the pre-US-6 `e2e` list green throughout (spec constraint).

- **E2E CI workflows.** New `.github/workflows/e2e.yml`: blocking PR smoke
  (rust-cache, `cargo build --release`, `just docker-func docker-visual`,
  then `func/smoke.sh`, `func/session-exit-status.sh`, `func/cli-smoke.sh`,
  `visual/titlebar.sh`; 40 min timeout, 25 min target) and an informational
  nightly full suite (`just e2e` + `just e2e-all-visual`) uploading
  `test-output/` artifacts on failure; flaky scripts get quarantine beads,
  never silent retries. Scope includes runner provisioning: install
  `just` and the client build apt deps (clang, libfontconfig-dev,
  libssl-dev, libwayland-dev, libx11-xcb-dev, libxkbcommon-x11-dev,
  libzstd-dev), mirroring release.yml's setup; the 25-min budget includes
  the apt step. Caching: `Swatinem/rust-cache` with `prefix-key: v1-rust`
  shared with release.yml (if compatible); buildx gha layer caching is
  IN, with named cache scopes `e2e-func` and `e2e-visual`. Runners are
  GPU-less `ubuntu-22.04`, which doubles
  as the US-6 portable-flags proof. No secrets are used anywhere in the
  workflow (P5). Rollback: the PR smoke can be demoted to non-blocking by
  config if it flakes. Depends on: full-suite/aggregate item; GPU/sentinel
  item. Acceptance: a PR run of the workflow passes within budget; a
  second warm-cache PR run is measured at ≤ 25 min; a manually
  dispatched nightly run produces the summary artifact; `lat.md/test.md`
  gains a named section (`## E2E CI`) covering the CI triggers,
  blocking-vs-informational semantics, and artifact location, and
  `lat check` passes. Completes US-6.

- **Sandbox limits, change-class taxonomy, and doc cross-references.** New
  `## Sandbox limits` section in `lat.md/test.md` (inserted after the
  Visual E2E section, ~line 580, with a ≤250-char leading paragraph):
  non-goals with sanctioned alternatives — real GPU (ask the user), macOS
  (ask the user), real multi-machine/mDNS/tailnet (`share-tap`/
  `share-inject`, `lan-peer`, `remote-peer` stand-ins), local-IPC version
  pairing (sanctioned check: `scribe-test remote-peer --refuse
  incompatible_version`; corrected citation per Risks), nushell/PowerShell
  (documented spec-018 limitation rows), instrumented builds (debug profile
  only), parallel same-host runs (unsupported), perf work
  (`tools/perf-ab-rig/run-perf-ab.sh` — a bare script, `--live` attaches to
  the isolated scribe-dev server, never restarts it), packaging
  (`tests/install/postinst-regressions.sh` rig); plus the change-class
  taxonomy (server, client rendering, CLI, protocol, persistence,
  packaging, AI launch, sharing/remote → harness path or non-goal); the
  release-workflows-don't-consume-e2e-images line; CLAUDE.md/AGENTS.md
  cross-reference. The section text can be drafted any time after the
  GPU/sentinel item, but the item closes last so the taxonomy reflects what
  actually landed. Depends on: keyring-rows item, full-suite item, CI item.
  Acceptance: `lat check` passes; every US-7 bullet present; the taxonomy
  audit runs as a checklist — each of {server, client rendering, CLI,
  protocol, persistence, packaging, AI launch, sharing/remote} maps to a
  named harness recipe or a named `## Sandbox limits` subsection.
  Completes US-7.

- **Follow-up bead (out of epic, filed at create-beads): hardened docker
  profile.** `--network none`, read-only rootfs, cap-drop for both images —
  explicitly excluded from this epic (Clarification Q3B); filed as a
  standalone bead so it is not lost. No dependencies inside this epic beyond
  landing after it.

## Backlog Refinement

None — no backlog sources for this run.

## Target Epic

New epic to be created at create-beads.

## Alignment fixes applied

Review-alignment fixes applied to this plan; one line per fix, tagged with its review ID and severity.

- A1 (must): image-plumbing acceptance now exercises the STALE branch (backdated mtime / trivial crates/ commit without rebuild) and requires the staleness rule as a comment in the docker-func/docker-visual recipes.
- M1+M2 (must): all entrypoint-func.sh edits except the start_session_keyring port moved to the GPU/sentinel item (item 2, where the recipe passthrough lands) — timeout override, sentinel export, PATH export; item 4 no longer edits the file; item 7 keeps only the keyring port; dependency prose updated.
- M3 (must): items 2, 4, and 10 acceptance now require named lat.md/test.md sections (`## E2E Recipe Contract`, `## AI-Launch Harness Plumbing`, `## E2E CI`) covering their new contracts plus a `lat check` pass.
- M4 (must): unowned `e2e-func-debug`/`e2e-visual-debug` wrapper recipes deleted from Affected Components; the `image=` recipe parameter covers the diagnosis flow.
- M5 (must): CI item gains runner-provisioning scope — install `just` and the client build apt deps (clang, libfontconfig-dev, libssl-dev, libwayland-dev, libx11-xcb-dev, libxkbcommon-x11-dev, libzstd-dev) mirroring release.yml; 25-min budget includes the apt step.
- A2 (should): shell-env item acceptance records that client-side ai_tab_cwd mode selection stays with the spec-018 GPUI unit suites (referenced via spec-018 verification, no suite named in spec-018 docs) and that the narrowing is recorded in specs/018-ai-tab-shell-env/verification.md and lat.md/test.md.
- A3 (should): full-func-suite item enumerates the 8 previously-omitted scripts and adds an acceptance diff of `ls tests/e2e/func/*.sh` against the `e2e` recipe body.
- A4 (should): `profile` recipe parameter constrained to exactly `release|debug`, erroring on anything else (Clarification Q5).
- A5 (should): GPU/sentinel item notes the entrypoint-visual.sh `SCRIBE_E2E_SANDBOX=1` export as the sanctioned exception to the "visual image gains nothing new" constraint.
- A6 (should): `lat refs`/grep sweep for stale recipe mentions (old --gpus invocations and the legacy e2e list) added to GPU/sentinel and full-suite acceptance.
- S1 (should): CI acceptance requires a second warm-cache PR run measured ≤25 min, names the `v1-rust` cargo cache prefix-key shared with release.yml, and commits buildx gha caching IN with named scopes (`e2e-func`, `e2e-visual`).
- S2 (should): rollback notes added — GPU/sentinel (single-line reverts), full-suite (quarantine-bead policy extends to the local e2e list), CI (PR smoke demotable to non-blocking by config).
- S3 (should): "bit-identical" acceptance replaced with capturing `docker images -q scribe-test-func:latest`, running `just docker-func profile=debug`, and asserting the release tag's image ID is unchanged.
- S4 (should): item 2 acceptance adds the `:ro` verification — named one-off `touch /tests/x` EROFS check or a `grep -c ':ro'` count matching the mount sites.
- S5 (should): item 8 names the mechanism — keyring-gated blocks appended to the ai-shell-env-{bash,zsh,fish}.sh scripts, skipped unless SCRIBE_KEYRING=1, with the exact rerun command stated.
- S6 (should): taxonomy audit made a checklist — each of {server, client rendering, CLI, protocol, persistence, packaging, AI launch, sharing/remote} maps to a named harness recipe or named Sandbox-limits subsection.
- S7 (should): item 2 notes the gpu_flags hunk is the active-blocker fix and is front-loaded within the item's commit stream.
