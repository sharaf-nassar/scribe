# Analysis: e2e-sandbox-gaps

Cross-artifact analysis of `spec.md` and `plan.md` against `constitution.md`
for the 019-e2e-sandbox-gaps feature. Report only — no spec or plan edits.

## Coverage Table

| User story / requirement | Covered by (plan section) | Status |
|---|---|---|
| US-1: Debug-build sandbox | "Image build plumbing: profile staging, debug tags, func-image additions" (staging dir, `-debug` tags, missing/stale errors) + "Debug-image smoke validation and diagnosis flow" (named smokes, restated `TEST_TIMEOUT` budgets, documented rerun flow) | full |
| US-2: AI-tab launch and shell-env coverage | "scribe-test AI-launch plumbing and claude stub" + "AI-tab shell-env matrix scripts (bash/zsh/fish, non-keyring rows)" + "Keyring-dependent AI-launch rows and spec-018 verification sync"; shells apt-installed via the image-plumbing item. Note: `ai_tab_cwd` is narrowed to the server-side contract (client-side mode selection stays with spec-018 GPUI unit suites), and the narrowing is explicitly recorded in verification.md and lat.md (alignment fix A2) | full |
| US-3: scribe-cli in the sandbox | "scribe-cli func smoke script" (`func/cli-smoke.sh`: profile/windows/action + no-socket exit behavior); binary COPY lands in the image-plumbing item; covered/not-covered CLI surface documented in lat.md | full |
| US-4: Local-IPC version-mismatch rig (DESCOPED) | Documentation item — "Sandbox limits, change-class taxonomy, and doc cross-references" records local-IPC version pairing as a non-goal with the sanctioned `remote-peer --refuse incompatible_version` check and the corrected spec-018 citation (per Risks) | full |
| US-5: Envelope persistence with a secret service | "Keyring fixture port and .envz persistence assertion" (port `start_session_keyring` behind `SCRIBE_KEYRING=1`, extend `func/env-persistence.sh`, narrow `lat.md/test.md:203` skip); packages land in the image-plumbing item; `TEST_TIMEOUT` override lands in the GPU/sentinel item | full |
| US-6: Complete recipes, portable flags, host-safety, CI | "GPU opt-in, read-only /tests, sentinel guard" + "Full func suite and aggregate visual recipe" (all 20 existing + new scripts enumerated; JSONL summary; concurrency limits documented) + "E2E CI workflows" (blocking PR smoke + informational nightly, quarantine-bead flake policy) | full |
| US-7: Documented residual limits and change-class taxonomy | "Sandbox limits, change-class taxonomy, and doc cross-references" (non-goals with sanctioned alternatives, verified perf-rig citation, taxonomy audit checklist, CLAUDE.md/AGENTS.md cross-reference, release-images line, `lat check`) | full |
| Goal: any change class validatable or named in US-7 taxonomy | "Sandbox limits, change-class taxonomy, and doc cross-references" — taxonomy audit runs as a checklist mapping each of 8 change classes to a named recipe or Sandbox-limits subsection | full |
| Goal: debug builds via recipe parameter, staging dir, `-debug` tags | "Image build plumbing…" + "Debug-image smoke validation and diagnosis flow" | full |
| Goal: spec-018 surface covered for bash/zsh/fish; nushell/PowerShell non-goals | AI-launch plumbing + shell-env matrix + keyring-rows items; non-goals recorded in the taxonomy item | full |
| Goal: scribe-cli shipped in func image and smoke-covered headless | Image-plumbing item (COPY) + "scribe-cli func smoke script" | full |
| Goal: `.envz` assertable via ported keyring fixture | "Keyring fixture port and .envz persistence assertion" | full |
| Goal: `just e2e` all-func, `e2e-all-visual` delegation, GPU opt-in, CI smoke + nightly | GPU/sentinel item + full-suite/aggregate item + CI workflows item | full |
| Goal: `/tests` read-only mount + in-container sentinel guard | "GPU opt-in, read-only /tests, sentinel guard" (`:ro` on all mounts, `SCRIBE_E2E_SANDBOX=1` exports, guard prologue on ~60 scripts, `grep -L` completeness check) | full |
| Goal: every accepted limit written into `lat.md/test.md` with alternative path | "Sandbox limits, change-class taxonomy, and doc cross-references" | full |
| Constraint: sandbox-only invariant (never touch host server) | Testing Strategy preamble — the harness validates itself; every work item names a docker-recipe verifying check; `socket.rs:11-19` has no override; perf-rig `--live` attaches without restart | full |
| Constraint: P4 budgets (func-image growth, debug-image size, wall-clocks, PR smoke) | "P4 budgets" subsection of Testing Strategy — numbers for all four axes plus debug smoke timeouts; runtime perf explicitly declared inapplicable | full |
| Constraint: cross-story ordering (plumbing first; US-5 → US-2 keyring rows; US-5 → US-6 all-func) | Sequencing — image-plumbing lands first; keyring-rows item depends on shell-env matrix + keyring fixture; full-suite item depends on keyring fixture; single item owns all Dockerfile/justfile plumbing | full |
| Clarification Q1 (US-4 descoped) | US-4 stub in spec + taxonomy item records the non-goal, sanctioned check, and corrected citation | full |
| Clarification Q2 (bash/zsh/fish matrix, plumbing in scope) | Image-plumbing item (apt shells) + AI-launch plumbing item + shell-env matrix item | full |
| Clarification Q3 (gap-7 non-goals; hardened-profile follow-up bead; `/tests:ro` + sentinel; taxonomy) | Taxonomy item (A, D) + GPU/sentinel item (C) + "Follow-up bead (out of epic, filed at create-beads): hardened docker profile" (B) | full |
| Clarification Q4 (blocking PR smoke + informational nightly, quarantine beads) | "E2E CI workflows" (named 4-script smoke subset, 25 min target / 40 min timeout, nightly artifacts, flake policy) | full |
| Clarification Q5 (staging dir, `-debug` tags, debug-only, staleness rule) | Image-plumbing item — `profile` constrained to exactly `release\|debug`, STALE branch exercised in acceptance, staleness rule commented in recipes (A1, A4) | full |
| Clarification Q6 (aggregate delegates to per-script recipes; all 8 omitted func scripts join) | Full-suite/aggregate item — 8 scripts enumerated, delegation loop, JSONL summary, `ls`-vs-recipe diff check (A3) | full |
| Clarification Q7 (port `start_session_keyring`; `TEST_TIMEOUT` override) | Keyring fixture item (port) + GPU/sentinel item (owns the `timeout "${TEST_TIMEOUT:-30}"` change per M1+M2 ownership split) | full |

Status counts: 25 full / 0 partial / 0 none.

## Backlog Disposition

| Source | Disposition | Target | Notes |
|---|---|---|---|
| None — no `source_backlog` or epic-closure P4 sources for this run | n/a | n/a | n/a |

## Target Epic

New epic to be created at create-beads.

## Remaining Risks

All carried from the plan's Risks section with mitigations in place; none are
blockers.

- **Debug-binary timeout flakiness.** The 702 MB debug client under lavapipe
  can exceed entrypoint helpers' internal 15–20 s waits, not just
  `TEST_TIMEOUT`. Mitigated by restated budgets (90 s func / 240 s visual)
  and keeping debug images out of CI; residual risk that internal helper
  timeouts (not env-tunable) still trip on slow hosts.
- **gnome-keyring-as-root quirks in the func image.** mlock/CAP_IPC_LOCK,
  `/run/user/0`, and `dbus-launch` living in `dbus-x11` are all handled by
  porting the working visual fixture verbatim, but the func container is a
  different runtime context; failure is contained to `SCRIBE_KEYRING=1`
  scripts.
- **`.envz` persist-trigger uncertainty.** The only production trigger is the
  debounced env-delta path with no flush-on-shutdown; a script that exits too
  fast loses the pending debounce. Mitigated by keep-alive + polling; the
  fallback oracle (server-log `EnvStatusState::Active` + `read_envelope` via
  a second session) is documented but unbuilt.
- **CI cold-cache first run.** The 40–90 min cold release build is exempt
  from the 25 min budget by design; the first PR run (and any
  `Swatinem/rust-cache` miss from lockfile churn) will be slow or hit the
  40 min job timeout, appearing as a red check until the cache warms.
- **Aggregate error-collection across ~30 recipes.** `just`-in-a-loop failure
  semantics are shell-flag sensitive; the per-iteration status-capture
  pattern plus the injected-failure acceptance check mitigate, but partial
  summaries on mid-loop crashes remain possible (append-per-script limits
  the loss).
- **Spec-018 citation defect baked into spec 019.** The US-4 descope cites
  `specs/018-ai-tab-shell-env/verification.md:75` for an "OUT OF CONTRACT"
  string that does not exist in that file (`:73` is an
  availability-limitation row). The descope itself is sound
  (`ClientMessage::Hello` has no version field, `protocol.rs:339`); the
  corrected citation is routed through the US-7 docs item rather than
  propagating the wrong one. Residual risk only if that work item skips the
  correction.
- **passwd-mutation fragility.** `usermod -s` relies on root + per-launch
  `getpwuid`; an nscd-style cache or a future non-root container breaks it.
  The `tier = "passwd"` log oracle detects breakage; the fallback
  (per-shell users + `setpriv`) is documented but unbuilt.
- **Sentinel sweep breadth.** ~60 scripts touched mechanically; a missed
  script stays host-runnable. The `grep -L SCRIBE_E2E_SANDBOX` acceptance
  check closes this, but it is not yet a standing `ready`-gate check.

## Unresolved Questions

None blocking. The three items the spec listed as planning-level decisions
were all decided in-plan: passwd mutation via `usermod -s` with the
`tier = "passwd"` log oracle; the `.envz` trigger is the shell-reported
env-delta through the debounced `schedule_persist` path; the PR smoke subset
is named (`func/smoke.sh`, `func/session-exit-status.sh`,
`func/cli-smoke.sh`, `visual/titlebar.sh`) with a 25 min target / 40 min
timeout. Two trivially deferred details remain for implementation:
the exact insertion point of `## Sandbox limits` ("~line 580") and whether
the keyring item narrows the `lat.md/test.md:203` skip to "nothing" or to
the no-flush-on-shutdown caveat — both are resolvable inside their beads
without re-planning.

## Constitution Check

- **P1 Clear Boundaries and Typed Failure — pass.** All changes stay inside
  existing crate/file responsibilities; the frozen local IPC protocol is
  untouched (additive fields already exist); the only new surface is the
  harness-internal same-binary daemon protocol.
- **P2 Session-Safe, Consistent UX — pass.** No user-facing UX changes; the
  feature protects session continuity by making the sandbox the sole
  validation path so the developer's live server is never disturbed.
- **P3 Explicit, Risk-Based Verification — pass (tension noted and
  mitigated).** The feature is verification infrastructure and test code is
  spec-requested; every work item names its verifying script/check. The
  deliberate tension — the nightly full suite is informational, not blocking
  — is mitigated by the blocking PR smoke and the quarantine-bead flake
  policy.
- **P4 Performance Budgets and Measurement — pass.** Numbers stated for
  func-image growth (≤ +150 MB), debug-image size (≤ release + 1.5 GB), PR
  smoke (≤ 25 min warm), nightly (2–3 h, 4 h timeout), debug smoke timeouts;
  runtime perf explicitly declared inapplicable with the perf A/B rig named.
- **P5 Default-Safe Trust Boundaries — pass.** The container remains the
  trust boundary; no secrets in images or CI (dummy empty keyring password
  only); `/tests` goes read-only and the sentinel guard closes host-touch
  paths.
- **P6 Local-First Data Locality — pass.** Nothing in the harness gains a
  runtime network dependency (apt-only build-time additions); the future
  hardened profile (`--network none`) is filed as a follow-up bead.
- **P7 Compatible, Documented, Operationally Safe Change — pass.** The live
  server is never touched or restarted (no socket override exists;
  perf-rig `--live` attaches without restart); `lat.md` sync and `lat check`
  ride inside each work item; compatibility decisions (no protocol bump,
  release workflows don't consume e2e images) are documented.

## Recommendation

**GO.** Every in-scope user story (US-1 through US-7, with US-4 correctly
descoped to a documentation obligation) traces to named plan work items with
concrete, verifiable acceptance criteria; all 8 spec Goals, the three key
Constraints, and all seven Clarifications are fully covered — 25/25 coverage
rows are full. No constitution principle is violated; the single P3 tension
(informational nightly) is explicitly acknowledged and mitigated by the
blocking PR smoke plus the quarantine-bead policy. The plan's review
alignment pass (A1–S7) already closed the ownership and acceptance gaps that
would otherwise have warranted fixes, and the known spec-citation defect is
self-correcting via the US-7 docs item. Remaining risks are operational
(flake, cold cache, keyring-as-root) with documented mitigations and cheap
rollbacks, not design gaps. Proceed to create-beads.
