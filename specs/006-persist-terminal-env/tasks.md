---

description: "Task list for feature 006 — Persist & Restore Terminal Environment Across Cold Restart"
---

# Tasks: Persist & Restore Terminal Environment Across Cold Restart

**Input**: Design documents from `/specs/006-persist-terminal-env/`
**Prerequisites**: `plan.md` (required), `spec.md` (required for user stories), `research.md`, `data-model.md`, `contracts/`

**Tests**: The plan documents surgical unit tests for high-risk isolated logic (delta computation, ExclusionSet, envelope round-trip, PreflightError mapping). The spec did **not** request a broad automated test surface; manual quickstart is the primary verification path per constitution principle II.

**Organization**: Tasks are grouped by user story so each story can be implemented and tested independently. P1 = MVP.

**Constitution Gates**: Generated tasks preserve the crate boundaries documented in `plan.md`, include per-story manual verification, reflect the UX and performance targets from `plan.md` and `quickstart.md`, and avoid any live Scribe-server restart (the feature is delivered via the existing `ConfigReloaded` flow + behavior changes at next session create/restore).

## Format: `[ID] [P?] [Story] Description with file path`

- **[P]**: Different files, no dependencies on incomplete tasks — safe to run in parallel.
- **[Story]**: `[US1]` / `[US2]` / `[US3]` for user-story phases only. Setup, Foundational, and Polish have no story label.

## Path Conventions

Multi-crate Rust workspace at the repository root. Paths are workspace-relative.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Add new workspace dependencies and create the new module skeleton so everything else has a place to land.

- [X] T001 Add the `keyring` workspace dependency and confirm or add an AEAD primitive (prefer `ring` if already transitively present in `Cargo.lock`, else add `chacha20poly1305` from RustCrypto) in `Cargo.toml`; pin both versions to current `crates.io` releases at implementation time
- [X] T002 Create the new module skeleton at `crates/scribe-server/src/env_store/mod.rs` with submodule declarations (`mod envelope; mod keystore; mod delta; mod store;`) and register the module in `crates/scribe-server/src/main.rs` (or `lib.rs` if present)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Shared protocol, config, hook, and module-level scaffolding that every user story depends on.

**⚠️ CRITICAL**: No user-story work can begin until this phase is complete.

- [X] T003 [P] Add `TerminalEnvPersistenceConfig { enabled: bool }` nested struct (`#[serde(default)]`, default `false`) and add `env_persistence: TerminalEnvPersistenceConfig` field (`#[serde(default)]`) to `TerminalConfig` in `crates/scribe-common/src/config.rs`
- [X] T004 [P] Add `HookEventKind::EnvChanged { added: Vec<(String, String)>, removed: Vec<String>, baseline_ready: bool }` variant (each field `#[serde(default)]`) to `crates/scribe-common/src/hook.rs`
- [X] T005 [P] Add `ClientMessage::EnvPreflight`, `ServerMessage::EnvPreflightResult { ok, error }`, `ServerMessage::EnvStatus { session_id, state }`, `enum PreflightError`, `enum EnvStatusState`, and the additive `env_envelope_id: Option<String>` field on `ClientMessage::CreateSession` to `crates/scribe-common/src/protocol.rs` (all new fields `#[serde(default)]`)
- [X] T006 Add the `ExclusionSet` constant (initial membership from `data-model.md::ExclusionSet`) in `crates/scribe-server/src/env_store/delta.rs` (depends on T002)
- [X] T007 Add the `--event env-delta` subcommand with `--added-json`, `--removed-json`, and `--baseline-ready` flags that emits `HookEventKind::EnvChanged` over the existing structured hook channel in `crates/scribe-hook-helper/src/main.rs` (depends on T004)
- [X] T008 Add a `"terminal.env_persistence.enabled"` match arm in `crates/scribe-settings/src/apply.rs::apply_terminal_key` that deserializes the bool and writes it to `config.terminal.env_persistence.enabled` via the existing `save_config` path (depends on T003)

**Checkpoint**: Protocol, config, hook variant, hook-helper subcommand, settings-apply hook, and the new module skeleton are in place. User stories can now begin.

---

## Phase 3: User Story 1 - Restore my environment after an unexpected cold restart (Priority: P1) 🎯 MVP

**Goal**: After a forced `scribe-server` death and respawn, each restored terminal comes back with the same user-set environment it had immediately before the restart. No process-/host-specific vars leak across.

**Independent Test**: Quickstart US1 — enable the feature (via direct config edit if US3 has not yet shipped); export several vars + change dir; `pkill -9 scribe-server`; verify restored terminal has the vars and the ExclusionSet correctly filtered out `SHLVL` / `WINDOWID` / `DISPLAY`.

### Implementation for User Story 1

- [X] T009 [P] [US1] Implement `TerminalEnvDelta` (`added: BTreeMap<String, String>`, `removed: BTreeSet<String>`), delta-against-baseline computation, `ExclusionSet` filtering, per-value 64 KiB and per-terminal 512 KiB caps with skip-with-log on overflow, and an `apply_event` method that folds an `EnvChangeEvent` into the delta — in `crates/scribe-server/src/env_store/delta.rs` (depends on T006)
- [X] T010 [P] [US1] Implement `StartupBaseline { vars, captured_at }` with a per-session capture-once invariant in `crates/scribe-server/src/env_store/mod.rs` (depends on T002)
- [X] T011 [US1] Implement `EnvEnvelope` AEAD seal/open with the versioned binary header (`version: u8 = 1`, 7-byte reserved zeros, 12-byte nonce, ciphertext + 16-byte Poly1305 tag) over `rmp_serde::to_vec_named` of `TerminalEnvDelta` in `crates/scribe-server/src/env_store/envelope.rs` (depends on T009)
- [X] T012 [P] [US1] Implement the OS-keystore wrapper around the `keyring` crate (flavor-aware identifier scheme via `AppIdentity` — `service` + `account = "env-key-{window_id}-{launch_id}"`; macOS login Keychain on macOS, Secret Service `login` collection on Linux) with `get_dek` / `set_dek` / `delete_dek` in `crates/scribe-server/src/env_store/keystore.rs` (depends on T002, T005)
- [X] T013 [US1] Implement `preflight()` (low-cost sentinel set + delete) and the `keyring::Error` → `PreflightError` mapping in `crates/scribe-server/src/env_store/keystore.rs` (depends on T012)
- [X] T014 [US1] Implement the on-disk envelope store (path `$XDG_STATE_HOME/<flavor>/restore/env/<window_id>/<launch_id>.envz`, 0o700 dirs, 0o600 files, atomic write-temp + rename, lifecycle `create`/`update`/`delete`) in `crates/scribe-server/src/env_store/store.rs` (depends on T011, T012)
- [X] T015 [US1] Implement the per-session 100 ms debounced persist scheduler (folds incoming `EnvChangedEvent`s into the live `TerminalEnvDelta`, re-encrypts the envelope, writes atomically via T014, leaves any existing envelope untouched on encryption failure) in `crates/scribe-server/src/env_store/mod.rs` (depends on T009, T010, T011, T014)
- [X] T016 [US1] Wire `HookEventKind::EnvChanged` translation in `crates/scribe-server/src/hook_ingress.rs`: on `baseline_ready: true` record the `StartupBaseline` for the session and clear any prior delta; on `baseline_ready: false` fold `added` / `removed` into the delta and reset/start the debounce timer; respect the `ExclusionSet` (depends on T004, T010, T015)
- [X] T017 [US1] In `crates/scribe-server/src/session_manager.rs`, when `CreateSession.env_envelope_id` is `Some` AND the feature is enabled AND the keystore is healthy: read the envelope file via T014, decrypt via T011, write the resulting `export NAME=value` / `unset NAME` statements (with shell-safe quoting) to a per-spawn 0o600 temp file under `$XDG_RUNTIME_DIR/<flavor>/env-apply/<session_id>-<pid>.sh`, and inject its absolute path as `SCRIBE_RESTORE_ENV_DELTA_FILE` in `build_pty_options.env`; schedule a grace-period unlink of the temp file as a defensive cleanup (depends on T014, T011)
- [X] T018 [P] [US1] Propagate the `env_envelope_id: Option<String>` field from `ClientMessage::CreateSession` through IPC dispatch into `session_manager::create_session(...)` in `crates/scribe-server/src/ipc_server.rs` (depends on T005; parallel with T017 — different files)
- [X] T019 [US1] Delete the envelope file (and any leftover `env-apply/` temp file for the session) on the clean session-close handler in `crates/scribe-server/src/ipc_server.rs` (depends on T014)
- [X] T020 [P] [US1] Set `env_envelope_id: Some(replay_launch.launch_id.clone())` when re-issuing a `LaunchRecord` via `CreateSession` in `crates/scribe-client/src/restore_replay.rs` (depends on T005)
- [X] T021 [P] [US1] Add the post-rc source block (sources `$SCRIBE_RESTORE_ENV_DELTA_FILE` if set, then `rm -f` it) + a `PROMPT_COMMAND`-driven env-delta emit + the one-shot tail `--baseline-ready` emit in `dist/shell-integration/bash/scribe.bash` (depends on T007)
- [X] T022 [P] [US1] Same three additions in `dist/shell-integration/zsh/scribe.zsh` (and as needed in `dist/shell-integration/zsh/.zshenv`) using `add-zsh-hook precmd` for the prompt-time hook (depends on T007)
- [X] T023 [P] [US1] Same three additions in `dist/shell-integration/fish/vendor_conf.d/scribe.fish` using a `fish_prompt` event handler (depends on T007)
- [X] T024 [P] [US1] Same three additions in `dist/shell-integration/nushell/vendor/autoload/scribe.nu` using the `pre_prompt` hook (depends on T007)
- [X] T025 [P] [US1] Same three additions in `dist/shell-integration/powershell/scribe.ps1` invoked from the `prompt` function (depends on T007)

**Checkpoint**: With the feature enabled (via direct config edit), capture + persist + restore works end-to-end on at least one shell. US1 is fully verifiable per `quickstart.md::US1`.

---

## Phase 4: User Story 2 - Changes are captured continuously and silently (Priority: P2)

**Goal**: The persisted record reflects the latest observed state without user action, including removals; no perceptible latency added to interactive commands.

**Independent Test**: Quickstart US2 — change vars at multiple points; inspect that the on-disk envelope is binary (not plaintext); confirm post-restart state reflects the latest change; A/B-compare 200-iteration wall-clock with feature off vs on within 5 % tolerance.

### Tests for User Story 2 (surgical unit tests per `plan.md` Constitution Gate 2)

- [X] T026 [P] [US2] Unit test for `TerminalEnvDelta::compute_against_baseline` covering add / modify / remove cases in `crates/scribe-server/src/env_store/delta.rs::tests`
- [X] T027 [P] [US2] Unit test for `ExclusionSet` filtering — every default-excluded name is dropped from a synthetic input delta; non-excluded names pass through — in `crates/scribe-server/src/env_store/delta.rs::tests`
- [X] T028 [P] [US2] Unit test for `EnvEnvelope` seal + open round-trip with a fixed-bytes sample key, asserting the binary header layout (`version`, nonce position, tag position) in `crates/scribe-server/src/env_store/envelope.rs::tests`

### Implementation for User Story 2

- [X] T029 [US2] Add a unit test that drives multiple `EnvChangedEvent`s through the persist scheduler within a single 100 ms window and asserts exactly one disk write occurs (coalescing) in `crates/scribe-server/src/env_store/mod.rs::tests` (depends on T015)
- [ ] T030 [US2] Manual: execute the latency-check procedure in `quickstart.md::US2` (200-iteration A/B with the feature off vs on); record the wall-clock numbers in the completion report. Pass if feature-on is within 5 % of baseline (depends on US1 implementation complete)

**Checkpoint**: Continuous capture freshness and the no-perceptible-latency property are verified; US1 still passes unchanged.

---

## Phase 5: User Story 3 - The feature is explicitly opt-in and sensitive values are protected (Priority: P3)

**Goal**: The feature is OFF by default; enabling runs a keystore preflight that refuses with an actionable message when prerequisites are missing; persisted data is encrypted at rest; runtime keystore failures fail safe (never plaintext) and surface non-intrusively in the status bar.

**Independent Test**: Quickstart US3 — Path A (happy path with keystore available): toggle on, verify persistence + restore + binary-only on-disk. Path B (failure path with keystore unavailable): toggle on → refused with inline error → no envelope written. Default-OFF check: with the toggle off, no behavior change vs today.

### Implementation for User Story 3

- [X] T031 [P] [US3] Add the "Persist Environment" toggle row + hidden inline `#env-persistence-error-row` to `crates/scribe-settings/src/assets/settings.html` immediately after the "Enhanced keyboard protocol (Kitty)" toggle, matching the existing `.setting-row` / `.setting-info` / `.toggle` structure with `data-key="terminal.env_persistence.enabled"`
- [X] T032 [P] [US3] Add the `.setting-error` CSS class (red text, semi-transparent red background, left border, 0.9 rem font, 1.4 line-height) to `crates/scribe-settings/src/assets/settings.css`
- [X] T033 [US3] Intercept the toggle click in `crates/scribe-settings/src/assets/settings.js`: on enable attempt send `ClientMessage::EnvPreflight`, await `EnvPreflightResult`; on `ok: true` flip the toggle visual + call existing `sendChange(...)` + hide the error row; on `ok: false` keep the toggle OFF + populate `#env-persistence-error-row` from the `PreflightError` → message map + auto-dismiss after 6 s. On disable click, flip to OFF and `sendChange(..., false)` unconditionally (depends on T031, T032)
- [X] T034 [P] [US3] Handle `ClientMessage::EnvPreflight` in `crates/scribe-server/src/ipc_server.rs`: call `env_store::keystore::preflight()` and reply with `ServerMessage::EnvPreflightResult { ok, error }` (depends on T013, T005)
- [X] T035 [US3] Handle the `terminal.env_persistence.enabled` transition on `ConfigReloaded` in `crates/scribe-server/src/ipc_server.rs`: on `false → true` initialize per-session env_store machinery for active sessions; on `true → false` stop all per-session persist timers AND delete every envelope under `$XDG_STATE_HOME/<flavor>/restore/env/` (depends on T014, T015)
- [X] T036 [P] [US3] Add per-session runtime `EnvStatus` tracking in `crates/scribe-server/src/env_store/mod.rs`: transition to `Degraded { reason }` on any keystore error during persist (leave any existing envelope untouched, write nothing in plaintext); transition back to `Active` only after a successful preflight; emit `ServerMessage::EnvStatus` to the owning client on each transition (depends on T015, T005)
- [X] T037 [P] [US3] Add `env_status: Option<EnvStatusState>` field to `Pane` in `crates/scribe-client/src/pane.rs` (depends on T005)
- [X] T038 [US3] Route `ServerMessage::EnvStatus` to the pane whose `session_id` matches and trigger a status-bar redraw in `crates/scribe-client/src/ipc_client.rs` (depends on T037, T005)
- [X] T039 [US3] In `crates/scribe-client/src/status_bar.rs`, when the focused pane's `env_status == Some(Degraded { .. })`, render a `⚠` warning glyph immediately right of the existing command-status indicator using the existing palette's warning slot; set the hover tooltip to "Environment capture paused: keystore unavailable. Retry from Settings → Terminal → General." (depends on T037)
- [X] T040 [P] [US3] Unit test for `PreflightError` mapping — each `keyring::Error` variant maps to the expected `PreflightError` variant — in `crates/scribe-server/src/env_store/keystore.rs::tests` (depends on T013)

**Checkpoint**: All three user stories pass their independent tests. The feature is shippable.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation sync, validation, and final cross-cutting tasks.

- [X] T041 [P] Update `lat.md/server.md` — add subsections for the env_store module under Sessions; for the EnvChanged hook event under Hook Channel; for the SCRIBE_RESTORE_ENV_DELTA_FILE env injection under Sessions/Session Creation; confirm the Handoff section's "what is preserved" list does NOT mention env (it is preserved by PTY-fd handoff, not by env_store)
- [X] T042 [P] Update `lat.md/client.md` — add `env_status` to Pane under the Restore Pipeline / Status Bar context; add `env_envelope_id` linkage under the Restore Pipeline context
- [X] T043 [P] Update `lat.md/settings.md` — add the new Terminal → General "Persist Environment" toggle row to the page contents description
- [X] T044 Run `lat check` and verify all wiki links and code refs resolve (depends on T041, T042, T043)
- [X] T045 Run `cargo test -p scribe-server env_store::` (and any other unit-test target introduced) and verify all unit tests pass (depends on T026, T027, T028, T029, T040)
- [ ] T046 Manual: execute the full `quickstart.md` (US1 + US2 + US3 Path A + US3 Path B + restore-timing measurement + failure-mode probes, **including the unsupported / disabled shell-integration scenario that explicitly verifies FR-010 + SC-006**) on Linux; if a macOS host is available, repeat there. Record platform(s), measurement numbers, and any deviations in the completion report.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies — can start immediately.
- **Foundational (Phase 2)**: depends on Setup completion — **blocks all user stories**.
- **User Stories (Phases 3–5)**: each depends on Foundational completion.
  - In a one-developer schedule: implement P1 → validate → P2 → validate → P3.
  - In a multi-developer schedule: P1 can run in parallel with P2 verification scaffolding and the UI/preflight parts of P3, since their files diverge after Foundational. The persist machinery (T009–T015) is owned by P1; P2 verifies its properties; P3 wraps it in the UI/preflight surface.
- **Polish (Phase 6)**: depends on the desired user-story phases being complete; `lat.md` updates can begin once Foundational is done (T041–T043 are documentation, the doc-only changes are safe earlier) but `lat check` (T044) needs the final code shape, so T044 runs at the end.

### Within-Phase Dependencies (summary; full chains are in each task line)

- Foundational: T006 ← T002; T007 ← T004; T008 ← T003.
- US1: T011 ← T009; T013 ← T012; T014 ← {T011, T012}; T015 ← {T009, T010, T011, T014}; T016 ← {T004, T010, T015}; T017 ← {T011, T014}; T018, T019 ← T005 / T014; T020 ← T005; T021–T025 ← T007.
- US2: T029 ← T015. T030 needs US1 implementation complete.
- US3: T033 ← T031, T032; T034 ← T013, T005; T035 ← T014, T015; T036 ← T015, T005; T037 ← T005; T038 ← T037, T005; T039 ← T037; T040 ← T013.
- Polish: T044 ← {T041, T042, T043}; T045 ← {T026, T027, T028, T029, T040}; T046 ← all user-story phases complete.

### Parallel Opportunities

- All Foundational tasks marked [P] (T003, T004, T005) can run in parallel — they touch independent files in `scribe-common`.
- Within US1, the parallel set is T009, T010, T012 (independent files in `env_store/`) plus T018 (`ipc_server.rs` — independent of `session_manager.rs`) plus T020 (client-side) plus T021–T025 (each shell script). T011, T013, T014, T015, T016, T017, T019 are sequential within their dependency chains.
- Within US2, T026, T027, T028 (unit tests, independent files) can run in parallel.
- Within US3, T031 (HTML), T032 (CSS), T034 (server preflight handler), T036 (runtime EnvStatus), T037 (client Pane field), T040 (preflight unit test) are marked [P] and independent of each other; T033 (JS) is sequential after T031 + T032 since it binds to both.
- Polish: T041, T042, T043 are independent `lat.md` files and can run in parallel before T044.

---

## Parallel Example: User Story 1 implementation kickoff (after Foundational)

```bash
# Independent files — start in parallel:
Task: "[US1] T009 — implement TerminalEnvDelta in crates/scribe-server/src/env_store/delta.rs"
Task: "[US1] T010 — implement StartupBaseline in crates/scribe-server/src/env_store/mod.rs"
Task: "[US1] T012 — implement keystore wrapper in crates/scribe-server/src/env_store/keystore.rs"
Task: "[US1] T020 — set env_envelope_id in crates/scribe-client/src/restore_replay.rs"
Task: "[US1] T021 — bash integration script additions under dist/"
Task: "[US1] T022 — zsh integration script additions under dist/"
Task: "[US1] T023 — fish integration script additions under dist/"
Task: "[US1] T024 — Nushell integration script additions under dist/"
Task: "[US1] T025 — PowerShell integration script additions under dist/"

# Then sequence the dependency chain (T011 needs T009; T014 needs T011+T012; T015 needs T009+T010+T011+T014; …)
```

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 1 — Setup (T001–T002).
2. Phase 2 — Foundational (T003–T008).
3. Phase 3 — User Story 1 (T009–T025).
4. **STOP and VALIDATE**: run quickstart.md US1 with the feature manually enabled in `config.toml`. Demo-ready as an MVP slice.

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. + User Story 1 → MVP behavior; demo via direct config edit; manual `quickstart.md::US1` passes.
3. + User Story 2 → freshness and latency properties verified by surgical unit tests + manual latency check.
4. + User Story 3 → opt-in UX, preflight, encryption guarantee, runtime fail-safe surface; manual `quickstart.md::US3` Path A and Path B both pass.
5. + Polish → `lat.md/` updated, `lat check` passes, full quickstart re-run on the target platform(s) and reported.

### Parallel Team Strategy

With multiple developers after Foundational completes:

1. Developer A: US1 persist machinery (T009–T019) — the densest dependency chain.
2. Developer B: shell integration (T021–T025), each in a separate file ⇒ trivially parallel.
3. Developer C: US3 UI + preflight (T031–T039) + the surgical unit tests (T026–T028, T040).
4. Integration point: T036 (runtime EnvStatus) needs T015; T034 (server preflight handler) needs T013. Both are clean handoffs.

---

## Notes

- `[P]` tasks operate on different files with no incomplete dependencies — safe to schedule concurrently.
- `[Story]` labels map tasks to user stories for traceability. Setup, Foundational, and Polish phases have no story label.
- Each user story is independently completable and verifiable per the corresponding `quickstart.md` section.
- The plan documents that unit tests are added **only** for the high-risk isolated units listed (delta computation, ExclusionSet, envelope round-trip, debounce coalescing, PreflightError mapping). No broader test harness is introduced (constitution principle II, documented deferral).
- Operational safety: no task requires a live-server restart. The feature is delivered via the existing `ConfigReloaded` flow (for the config field) and at next session create/restore (for behavior).
- Worktree discipline: the plan touches only feature-scoped files; do not amend unrelated files in the same commits.
- Constitution post-task gate: T041–T044 update `lat.md/` and run `lat check`; T046 records the manual quickstart results in the completion report.
- Avoid: vague task descriptions, same-file `[P]` conflicts, cross-story dependencies that break independent testability.

## Post-implementation follow-up

- **2026-05-19** — Closed the webview-to-server bridge gap left after T031–T034. `crates/scribe-settings/src/assets/settings.js` was already issuing `sendIpc({type: "env_preflight"})` and registering `window.SCRIBE_ON_ENV_PREFLIGHT_RESULT`, and `scribe-server` was already answering `ClientMessage::EnvPreflight`, but no Rust code in `scribe-settings` wired the two sides together. Mirroring the existing `release_list` bridge, this adds:
  - `server_action::request_env_preflight(Duration) -> EnvPreflightOutcome` (+ `parse_env_preflight_response` parser and two parser tests in `crates/scribe-settings/src/server_action.rs`).
  - `ENV_PREFLIGHT_TIMEOUT`, `env_preflight_payload_json` (manual JSON because `PreflightError::Unknown(String)` is a tuple variant `serde_json` rejects), `preflight_error_json`, `inject_env_preflight_result`, `dispatch_env_preflight_request_linux`, `dispatch_env_preflight_request_macos`, a new `TaoUserEvent::EnvPreflightResult` variant, a new `LinuxWebviewContext::active_env_preflight_source` cell with shutdown cancellation, and a new `SettingsIpcHandlers::on_request_env_preflight` slot wired into both `build_linux_webview` and `build_tao_webview` in `crates/scribe-settings/src/lib.rs`, plus an `env_preflight_payload_json_matches_contract` unit test.
- The settings.js kind string (`"env_preflight"`) and callback name (`SCRIBE_ON_ENV_PREFLIGHT_RESULT`) are the contract — Rust was adapted to match; settings.js was not modified. The msgpack wire format (`rmp_serde`) still handles the tuple-variant `PreflightError` cleanly because the contract violation is only on the webview-bound JSON.
