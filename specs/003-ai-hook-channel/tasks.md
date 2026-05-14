---

description: "Task list for AI Hook Channel — feature 003-ai-hook-channel"
---

# Tasks: AI Hook Channel

**Input**: Design documents from `/specs/003-ai-hook-channel/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/](./contracts/), [quickstart.md](./quickstart.md)

**Tests**: Included by default for this feature. The plan's research.md Decision 9 calls for `cargo test` unit/integration plus `tests/install/ipc-hook-regressions.sh` offline shell regressions. The Rust classifier and the helper's gating logic deserve unit tests because the spec's success criteria (SC-001, SC-005, SC-007) demand behavioral guarantees that are hard to verify by manual quickstart alone.

**Organization**: Tasks are grouped by user story (US1–US4 from [spec.md](./spec.md)). Each user-story phase is independently testable. **MVP = Phase 1 + Phase 2 + Phase 3 (US1)**; Phases 4–6 add the other providers, the extensibility verification, and the safety contract.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelizable with other [P]-marked tasks in the same phase (different files, no incomplete-dependency conflicts)
- **[Story]**: US1, US2, US3, US4 (only on user-story phase tasks; setup/foundational/polish are unlabeled)
- File paths are absolute or repo-relative depending on context

## Path Conventions

This is a multi-crate Rust workspace (see [plan.md](./plan.md) Project Structure). Paths are repo-root-relative. Crates live under `crates/`. Shell adapters and hook scripts live under `dist/`. Tests live under `crates/*/src/` (inline `#[cfg(test)]`), `crates/*/tests/` (integration), and `tests/install/` (offline shell regressions).

---

## Phase 1: Setup

**Purpose**: Verify branch state and prepare the workspace. Minimal because this is an existing Rust workspace, not a greenfield project.

- [x] T001 Confirm `git rev-parse --abbrev-ref HEAD` returns `003-ai-hook-channel`; abort if not, do not proceed
- [x] T002 Run `cargo check --workspace` to confirm the workspace builds cleanly before any changes land

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Wire types, server dispatch path, env-var injection, and crate skeleton. Once complete, every user story can be developed and tested independently.

**⚠️ CRITICAL**: No user story tasks (Phases 3–6) can begin until this phase finishes.

- [x] T003 [P] Create new module `crates/scribe-common/src/hook.rs` with `HookEvent`, `HookEventKind`, and per-field cap constants (`LAST_MESSAGE_CAP_BYTES = 16384`, `PROMPT_TEXT_CAP_BYTES = 256`, `TASK_LABEL_CAP_BYTES = 256`) per [data-model.md](./data-model.md). Re-export from `crates/scribe-common/src/lib.rs`.
- [x] T004 Add `ClientMessage::HookEvent(HookEvent)` variant to `crates/scribe-common/src/protocol.rs` (around line 211, after the existing variants). Follow the existing `#[serde(tag = "type", rename_all = "snake_case")]` pattern. Depends on T003.
- [x] T005 [P] Create new binary crate `crates/scribe-hook-helper/` with `Cargo.toml` and skeleton `src/main.rs`. Add to workspace `members` in root `Cargo.toml`. Configure `[profile.release]` `panic = "abort"` and `strip = true` if not inherited.
- [x] T006 [P] Add `SCRIBE_HOOK_SOCK` and `SCRIBE_SESSION_ID` to the env HashMap built at `crates/scribe-server/src/session_manager.rs:538` inside `build_pty_options`. Set `SCRIBE_HOOK_SOCK` to the server's existing socket path (passed in from `start_ipc_server`); set `SCRIBE_SESSION_ID` to the per-session UUID minted at `:298`. Follow [contracts/env-vars.md](./contracts/env-vars.md).
- [x] T007 Create new module `crates/scribe-server/src/hook_ingress.rs` with empty `pub async fn handle(ctx: &mut ClientContext, event: HookEvent)` stub plus the validation skeleton (look up session in `LiveSessionRegistry`, drop on miss). Depends on T003, T004.
- [x] T008 [P] Create new module `crates/scribe-server/src/stop_classifier.rs` with empty `pub fn classify(last_message: &str) -> AiState` stub returning `AiState::IdlePrompt` so it compiles; real heuristic comes in T020.
- [x] T009 Wire `ClientMessage::HookEvent` dispatch into `run_client_message_loop` at `crates/scribe-server/src/ipc_server.rs:611` following the existing `CheckForUpdates`/`ListReleases` transient-branch pattern (`:519-533`). Calls `hook_ingress::handle` then `break;` so the connection closes after one event. Depends on T004, T007.
- [x] T010 Pass the server socket path into `SessionManager` so T006 can use it. Trace from `crates/scribe-server/src/main.rs` (or wherever `start_ipc_server` is called) → `SessionManager::new` → `build_pty_options`. Add a field or constructor arg as needed.

**Checkpoint**: At end of Phase 2 — `cargo check --workspace` is clean, `ClientMessage::HookEvent` round-trips on the wire (no server handling yet beyond the stub), env vars reach a child shell launched in a Scribe pane (manual check: `echo "$SCRIBE_HOOK_SOCK $SCRIBE_SESSION_ID"`). User-story phases can begin.

---

## Phase 3: User Story 1 — Claude Code feature parity restored (Priority: P1) 🎯 MVP

**Goal**: A user running Claude Code inside Scribe gets back the tab indicator, prompt bar, and AskUserQuestion functionality that broke in CC v2.1.139.

**Independent Test**: Quickstart scenarios 1a–1d in [quickstart.md](./quickstart.md). After this phase: launch CC in a Scribe pane, trigger AskUserQuestion, observe picker rendering and selection round-trip; submit a prompt and watch the indicator transition through Processing → IdlePrompt or WaitingForInput per classifier.

### Tests for User Story 1

- [x] T011 [P] [US1] Inline `#[cfg(test)]` tests in `crates/scribe-server/src/stop_classifier.rs` covering every heuristic rule from [research.md](./research.md) Decision 5: trailing `?` detection, question phrases, approval/review phrases, code-fence stripping, "last 20 non-empty lines" window, default-to-idle fallback. Each rule = one named test fn.
- [x] T012 [P] [US1] Unit tests in `crates/scribe-hook-helper/src/main.rs` for arg parsing, env-var gating (both vars present, only one set, neither set, malformed UUID), and `HookEvent` construction per [contracts/helper-cli.md](./contracts/helper-cli.md). No I/O in these tests — gate on `Result<HookEvent, _>`.
- [ ] T013 [P] [US1] Integration test in new file `crates/scribe-server/tests/hook_channel_roundtrip.rs`. Pattern: model on `crates/scribe-server/tests/replay_roundtrip.rs`. Spawn in-process server, connect a client via `scribe_test::ipc`, send each `HookEventKind` variant, assert the matching `ServerMessage` arrives. Covers `StateChanged`, `SessionStopped` (with classifier-driven outcome), `PromptReceived`, `TaskLabelChanged`/`Cleared`, `ContextChanged`, `StateCleared` — and asserts the helper's connection receives no reply.
- [ ] T014 [P] [US1] Offline regression file `tests/install/ipc-hook-regressions.sh`, model on `tests/install/codex-context-regressions.sh`. Mocks `scribe-hook-helper` with a `/bin/sh` arg-echo, feeds each adapter sample stdin JSON, asserts the adapter exec'd the mock with expected flags and exited 0.

### Implementation for User Story 1

- [x] T015 [US1] Implement `scribe-hook-helper` `main()` in `crates/scribe-hook-helper/src/main.rs`. Order: install panic hook that swallows messages → read env vars → parse CLI args (manual or `clap` minimal) → build `HookEvent` → connect to socket via `tokio::net::UnixStream` with a `tokio::time::timeout(Duration::from_millis(100), ...)` over connect+write+flush → `framing::write_message(stream, &ClientMessage::HookEvent(event))` → close → `std::process::exit(0)`. Every fallible call uses `let _ = ...`; no `?` propagation; `eprintln!`/`println!` forbidden. Satisfies [contracts/helper-cli.md](./contracts/helper-cli.md) failure-mode table.
- [x] T016 [US1] Implement `hook_ingress::handle` in `crates/scribe-server/src/hook_ingress.rs`: validate session_id, build the right `MetadataEvent` per the mapping table in [data-model.md](./data-model.md) (different paths for `TaskLabelChanged` based on `provider == CodexCode`), invoke existing `send_metadata_event` at `ipc_server.rs:2615`. For `SessionStopped`, call `stop_classifier::classify` then emit `MetadataEvent::AiStateChanged`. For `ContextChanged`, use `send_ai_context_change` (`ipc_server.rs:2509-2532`).
- [x] T017 [US1] Implement `stop_classifier::classify` in `crates/scribe-server/src/stop_classifier.rs`. Port the three regex passes from `dist/detect-claude-question.sh:40-49`: (1) code-fence strip, (2) take last ~20 non-empty lines, (3) trailing-`?` then question-phrase then approval-phrase regex. Use `regex` crate (already in workspace via tokio's transitive deps; verify with `cargo tree`, add explicit dep if not). Return `AiState::WaitingForInput` on match, `AiState::IdlePrompt` otherwise. Make T011 pass.
- [x] T018 [P] [US1] Write `dist/ai-hook-claude.sh` (~20 lines). Reads stdin JSON, extracts `session_id`, `prompt`, `tool_name`, and last-assistant message for the appropriate event using `python3 -c '...'` (existing pattern from `dist/setup-claude-hooks.sh:89`). Picks `--event` based on `$1` (the hook event name passed by CC). For session-stopped, writes the last message to `mktemp` and passes `--last-message-file`. Exec's `scribe-hook-helper`. All-stderr-redirected-to-/dev/null per [contracts/helper-cli.md](./contracts/helper-cli.md).
- [x] T019 [P] [US1] Write `dist/ai-hook-statusline.sh` (~15 lines). Reads CC statusline JSON on stdin, extracts `context_window.used_percentage`, exec's `scribe-hook-helper --provider=claude_code --event=context_changed --fill-percent=N`. Writes a human banner to stdout exactly like the old `scribe-claude-statusline.sh:76-80` did (the banner IS the statusline display CC shows the user — different output channel from the hook events).
- [x] T020 [US1] Rewrite `dist/setup-claude-hooks.sh`. Replace the embedded `printf > /dev/tty` hook commands at `:77-89` with adapter invocations: each hook command becomes `/usr/share/scribe/ai-hook-claude.sh <event_name>`. Remove the `detect-claude-question.sh` install step. Add `ai-hook-statusline.sh` registration in CC's `statusLine` config. Preserve the `is_scribe_hook`/merge logic so existing user hooks survive. Depends on T018, T019.
- [x] T021 [US1] Remove AI hook OSC parsing from `crates/scribe-pty/src/metadata.rs`: delete `parse_named_ai_state`, `parse_named_ai_context`, `parse_provider_iterm2_payload`'s state/prompt/task-label branches, and the dispatch entries in `process_osc` that route to them. **KEEP** `AiProviderArmed` parsing (`ScribeAiLaunch=...` pre-arm sentinel) per FR-023. Verify with `cargo check --workspace`.
- [x] T022 [US1] Delete `dist/detect-claude-question.sh` and `dist/scribe-claude-statusline.sh`. Update `crates/scribe-server/Cargo.toml:68-112` to remove the deb-asset entries for those files and add entries for `scribe-hook-helper`, `dist/ai-hook-claude.sh`, `dist/ai-hook-statusline.sh`. Mirror in the `scribe-dev` block at `:185-228`.
- [x] T023 [US1] Update `dist/macos/build-dmg.sh:123-131` to copy `scribe-hook-helper` and the new adapter scripts; remove the now-deleted ones.
- [x] T024 [US1] Update `lat.md/pty.md`: **remove** the sections "OSC 1337 — AI State", "OSC 1337 — AI Context Refresh", "OSC 1337 — Prompt Text" (these are gone from the parser). **Keep** "OSC 1337 — Pre-Arm Sentinel". Add a cross-reference to the new `lat.md/server.md#Hook Channel` section (created in T025).
- [x] T025 [US1] Update `lat.md/server.md`: add new "Hook Channel" subsection under `## Server` documenting the ingress, env-var injection, and the stop classifier. Cross-link to `[[crates/scribe-server/src/hook_ingress.rs#handle]]`, `[[crates/scribe-server/src/stop_classifier.rs#classify]]`, and `[[crates/scribe-common/src/hook.rs#HookEvent]]`. Add `// @lat:` ref comments at those Rust call sites.
- [x] T026 [US1] Update `lat.md/architecture.md` Crate Map: add new entry for `scribe-hook-helper` after `scribe-cli`. Brief: "Tiny binary invoked by AI-tool-installed hook adapter scripts to emit one `HookEvent` to the running server."
- [x] T027 [US1] Run `lat check` from repo root; resolve any broken refs introduced by T024–T026.

**Checkpoint**: User Story 1 fully functional. Test independently per quickstart scenario 1 (subscenarios 1a–1d). Codex and Auggie still on the old broken path — that is Phase 4.

---

## Phase 4: User Story 2 — Codex and Auggie surface parity (Priority: P2)

**Goal**: Codex and Auggie sessions in Scribe gain the same tab-indicator and prompt-bar behavior on the new channel, with no per-provider transport divergence.

**Independent Test**: Quickstart scenarios 2a–2c. After this phase: state transitions, prompt bar, and task labels work identically across Claude, Codex, and Auggie sessions.

### Tests for User Story 2

- [ ] T028 [P] [US2] Extend `tests/install/ipc-hook-regressions.sh` (created in T014) with cases for `dist/ai-hook-codex.sh` and `dist/ai-hook-auggie.sh`: feed sample Codex hook JSON and Auggie hook JSON, assert the helper mock receives the right flags. Confirm the script still exits 0 on missing fields, missing `python3`, etc.
- [ ] T029 [P] [US2] Add a roundtrip case to `crates/scribe-server/tests/hook_channel_roundtrip.rs` that sends `HookEvent { provider: CodexCode, kind: TaskLabelChanged { … } }` and asserts a `ServerMessage::CodexTaskLabelChanged` arrives (not `TaskLabelChanged` — verifies the provider-aware routing in T016).

### Implementation for User Story 2

- [x] T030 [P] [US2] Write `dist/ai-hook-codex.sh` modeled on `dist/ai-hook-claude.sh` from T018, but extracting Codex's hook payload fields (different JSON schema — consult `dist/codex-prompt-state.sh`, `dist/detect-codex-question.sh`, `dist/codex-task-label.sh` for the field names before deleting them in T034). Note Codex's task-label flow uses a different event matcher than Claude.
- [x] T031 [P] [US2] Write `dist/ai-hook-auggie.sh` modeled on the Auggie hook flow (consult `dist/auggie-state.sh` and `dist/setup-auggie-hooks.sh`). Auggie emits prompts from the `Stop` hook (per `includeConversationData` docs) — adapter must call `--event=prompt_received` AND `--event=session_stopped` from the same Stop fire if both apply.
- [x] T032 [US2] Rewrite `dist/setup-codex-hooks.sh`. Replace all `~/.codex/hooks.json` entries that invoked `codex-hook-common.sh`, `codex-prompt-state.sh`, `detect-codex-question.sh`, `codex-task-label.sh`, `detect-codex-context.sh` with adapter invocations: `/usr/share/scribe/ai-hook-codex.sh <event_name>`. Preserve the `~/.codex/config.toml` `[features].hooks = true` toggle. Depends on T030.
- [x] T033 [US2] Rewrite `dist/setup-auggie-hooks.sh` analogously. Depends on T031.
- [x] T034 [US2] Delete the now-obsolete shell scripts: `dist/codex-hook-common.sh`, `dist/codex-prompt-state.sh`, `dist/codex-task-label.sh`, `dist/detect-codex-question.sh`, `dist/detect-codex-context.sh`, `dist/auggie-state.sh`. Remove their entries from `crates/scribe-server/Cargo.toml` deb-asset tables (both stable and dev) and `dist/macos/build-dmg.sh`.
- [x] T035 [US2] Update `dist/debian/postinst` if any reference to the deleted scripts exists (search for `codex-hook-common`, `codex-prompt-state`, `detect-codex-context`, etc.). The `setup-{claude,codex,auggie}-hooks.sh` invocations at `:680/:698/:716` stay; only auxiliary path references need cleanup.
- [x] T036 [US2] Update `lat.md/pty.md` "OSC 1337 — AI State" / "Prompt Text" sections were removed in T024; confirm no stale Codex/Auggie OSC references survive. Update `lat.md/common.md` AI State section to note that state now arrives via the hook channel, not OSC, for all three providers.
- [x] T037 [US2] Re-run `lat check`; resolve broken refs.

**Checkpoint**: User Stories 1 AND 2 both work independently. All three production providers route via the new channel. Quickstart 2a–2c pass.

---

## Phase 5: User Story 3 — Adding a new AI provider (Priority: P3)

**Goal**: Verify FR-018 — the architecture supports a new provider with only a new adapter script. This phase is partly a verification exercise and partly documentation.

**Independent Test**: Quickstart scenario 3a — the manual "Foo" provider walkthrough. The exercise confirms that no transport, helper, env-var, or server-consumer code needs to change to add a fourth provider.

### Tests for User Story 3

- [ ] T038 [P] [US3] Add a roundtrip case to `crates/scribe-server/tests/hook_channel_roundtrip.rs` that constructs a `HookEvent` with a `provider` value not in `AiProvider::all()` (using a hand-crafted msgpack payload that bypasses the enum at deserialization, or by temporarily extending `AiProvider` behind `#[cfg(test)]`). Assert the server drops it silently and other in-flight events still process. Verifies FR-014.

### Implementation for User Story 3

- [x] T039 [US3] Document the "adding a provider" procedure in a new section in `lat.md/server.md#Hook Channel#Adding a Provider`: enumerate the steps (one `AiProvider` enum variant, one adapter script, one entry in `dist/setup-<provider>-hooks.sh`, one entry in deb/dmg asset tables — nothing else). Include the quickstart 3a snippet as a worked example.

**Checkpoint**: FR-018 verified by execution. Future providers follow the documented path.

---

## Phase 6: User Story 4 — Hooks run safely outside Scribe (Priority: P2)

**Goal**: Confirm helpers and adapters never break the AI tool outside Scribe contexts. Most of the helper-side gating is already in T015 (US1), but US4 adds dedicated tests that pin the FR-025 safety contract.

**Independent Test**: Quickstart scenarios 4a–4e. After this phase: the helper exits 0 with zero I/O in every non-Scribe surface (cloud session, SSH, CI, missing env vars, dead socket, missing helper binary).

### Tests for User Story 4

- [ ] T040 [P] [US4] Extend the unit tests in T012 with explicit cases: helper invoked with `SCRIBE_HOOK_SOCK` set to `/tmp/nonexistent.sock` exits 0 within the 100 ms budget; helper invoked with malformed UUID in `SCRIBE_SESSION_ID` exits 0; helper invoked with both env vars unset exits 0 without touching the args; helper invoked through `sh -c '… helper …'` (one level of subshell) and through `env -i SCRIBE_HOOK_SOCK=… SCRIBE_SESSION_ID=… sh -c '… helper …'` (env scrubbed except for the two Scribe vars) behaves identically — covers FR-024 subshell / wrapper propagation.
- [ ] T041 [P] [US4] Extend `tests/install/ipc-hook-regressions.sh` (T014) with `unset SCRIBE_HOOK_SOCK SCRIBE_SESSION_ID` cases for every adapter (`ai-hook-claude.sh`, `ai-hook-codex.sh`, `ai-hook-auggie.sh`, `ai-hook-statusline.sh`); assert each exits 0 with empty stdout and empty stderr. Also add a wrapped-invocation case (`sh -c 'env … ai-hook-claude.sh'`) verifying that one level of subshell preserves env-var inheritance — covers FR-024.
- [ ] T042 [P] [US4] Add a "no stderr" assertion to the integration test in T013: capture stderr of the test's helper invocations (when run in a context with stale env) and assert it is empty bytewise.

### Implementation for User Story 4

- [ ] T043 [US4] If any of the tests in T040–T042 reveal a leakage path (panic message, `?` propagation, accidental `eprintln!`), patch `crates/scribe-hook-helper/src/main.rs` to plug it. Re-run `cargo clippy -- -W clippy::print_stderr -W clippy::print_stdout` to catch print statements at compile time.
- [x] T044 [US4] Document the safety contract in a new `lat.md/server.md#Hook Channel#Safety Contract` subsection: enumerate FR-003, FR-007 through FR-011, the 100 ms timeout (FR-012), and quickstart 4a–4e as the verification checklist.

**Checkpoint**: FR-025 holds. All four user stories independently functional.

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Final sweep — documentation, packaging, lint cleanliness, lat.md sync.

- [x] T045 [P] Update `lat.md/protocol.md`: add the new `ClientMessage::HookEvent` variant and the `HookEvent` / `HookEventKind` types to the documented protocol surface. Cross-link to `[[crates/scribe-common/src/hook.rs#HookEvent]]` and `[[crates/scribe-common/src/protocol.rs#ClientMessage]]`.
- [x] T046 [P] Update `lat.md/test.md`: add new section "Hook Channel Roundtrip Tests" describing the integration test pattern; add entry for `tests/install/ipc-hook-regressions.sh` under "Installer Script Regression Harness".
- [x] T047 [P] Update `justfile`: if `setup-claude`/`setup-codex`/`setup-auggie` targets reference deleted scripts, update them. Add a `scribe-hook-helper-bin` target if convenient for dev workflow.
- [x] T048 Run `cargo clippy --workspace -- -D warnings` and resolve any new lints introduced by this feature. NO new `#[allow]` or `#[expect]` suppressions per `tools/check-no-new-lint-suppressions.sh`.
- [x] T049 Run `tools/check-no-new-lint-suppressions.sh` and confirm zero new entries.
- [x] T050 Run `lat check` from repo root; resolve any remaining broken refs across all `lat.md/` updates from T024–T026, T036, T039, T044, T045, T046.
- [ ] T051 [P] Run the `tests/install/` regression suite end-to-end (`bash tests/install/postinst-regressions.sh && bash tests/install/ipc-hook-regressions.sh && …`); confirm all pass.
- [x] T052 [P] Run `cargo test --workspace`; confirm all pass.
- [ ] T053 Measure hook-fire-to-`AiStateChanged` latency under synthetic load (SC-002 enforcement). Spawn `scribe-test ipc-trace --filter AiStateChanged --duration 60` in the background and drive each `HookEventKind` variant via `scribe-hook-helper` at 5-second intervals against an in-process server (reuse the `crates/scribe-server/tests/replay_roundtrip.rs` pattern; gate behind `cargo test --release -- --ignored perf_latency`). Compute p95 from the trace. **Fail the implementation if p95 > 200 ms.** Record the measured value as a comment in `plan.md`'s Performance Goals section or in an implementation summary.
- [x] T054 Execute every scenario in [quickstart.md](./quickstart.md) manually. Tick off each acceptance check. Treat any failure as blocking.
- [ ] T055 Confirm SC-007 / FR-022 / SC-008 with three greps: (a) `grep -RIn '> /dev/tty' dist/` returns no matches (SC-007); (b) `grep -nE 'ClaudeState\|CodexState\|AuggieState' crates/scribe-pty/src/metadata.rs` is empty (FR-022); (c) `grep -RInE 'ClaudeState=\|CodexState=\|AuggieState=\|ClaudePrompt=\|CodexPrompt=\|AuggiePrompt=\|ClaudeTaskLabel=\|CodexTaskLabel=' dist/ai-hook-*.sh dist/setup-*-hooks.sh` returns no matches (SC-008 zero-duplication of state-emission strings in adapter / installer scripts). Note: `dist/shell-integration/` may still contain `ScribeAiLaunch=` references per FR-023; that is expected and out of scope.
- [ ] T056 Ask the user for explicit approval before `just restart-server` (CLAUDE.md mandates this). If approved, restart and run quickstart scenarios with the fresh server. If not approved, document that the next user restart will pick up the changes.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)** → no dependencies; trivial branch and build sanity.
- **Foundational (Phase 2)** → depends on Setup. **BLOCKS** all user-story phases. Wires types, server dispatch, env injection.
- **Phase 3 (US1, MVP)** → depends on Foundational. Independent of US2/US3/US4.
- **Phase 4 (US2)** → depends on Foundational (could also start in parallel with Phase 3 but shares the helper, regression file, and lat.md surface — easier to land sequentially).
- **Phase 5 (US3)** → depends on Phase 3 + Phase 4 (the exercise verifies the architecture with all production providers landed).
- **Phase 6 (US4)** → depends on Phase 3 (the helper is the surface under test). Can land before US2/US3 if desired.
- **Phase 7 (Polish)** → depends on all user-story phases complete.

### Within Each Phase

- **Phase 2 (Foundational)**: T003 → T004 (variant depends on types) → T005, T006 [P], T007 (depends on T003, T004), T008 [P], T010 → T009 (dispatch depends on ingress stub + variant).
- **Phase 3 (US1)**: tests T011–T014 [P] first; then implementation T015 (helper main), T017 (classifier), T016 (ingress) — these three can run in parallel since they're different files. T018, T019 [P] then T020 (setup script depends on adapters). T021 (delete OSC parsing) can run any time after T016. T022, T023 (packaging) after T018–T019. T024–T026 [P] (lat.md updates). T027 (lat check) last.
- **Phase 4 (US2)**: tests T028, T029 [P] first; T030, T031 [P]; T032 depends on T030; T033 depends on T031; T034 after both; T035 cleanup; T036, T037 lat.
- **Phase 5 (US3)**: T038, T039 are independent and can run in either order.
- **Phase 6 (US4)**: T040, T041, T042 [P] first; T043 if any tests fail; T044 lat.
- **Phase 7**: T045–T047 [P]; T048, T049 sequential (lint chain); T050 lat; T051, T052 [P]; T053 latency measurement; T054 manual quickstart; T055 final greps; T056 last (requires user approval).

### Parallel Opportunities Summary

- **Foundational**: T003 + T005 + T008 can run together. T006 (env injection) is independent of types and can run in parallel too.
- **US1**: All four test tasks (T011, T012, T013, T014) parallelizable; helper / ingress / classifier (T015, T016, T017) parallelizable; adapter scripts (T018, T019) parallelizable; lat docs (T024, T025, T026) parallelizable.
- **US2**: T028 + T029 [P]; T030 + T031 [P].
- **US4**: T040 + T041 + T042 [P].
- **Polish**: T045 + T046 + T047 [P]; T051 + T052 [P].

---

## Parallel Example: User Story 1

```text
# After Phase 2 checkpoint, launch in parallel:
Task T011 — stop_classifier unit tests
Task T012 — helper unit tests
Task T013 — hook_channel_roundtrip integration test
Task T014 — tests/install/ipc-hook-regressions.sh

# Then in parallel:
Task T015 — implement scribe-hook-helper main
Task T016 — implement hook_ingress::handle
Task T017 — implement stop_classifier::classify

# Then in parallel:
Task T018 — write ai-hook-claude.sh
Task T019 — write ai-hook-statusline.sh

# Then in parallel:
Task T024 — lat.md/pty.md updates
Task T025 — lat.md/server.md updates
Task T026 — lat.md/architecture.md updates
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1 (Setup) — trivial.
2. Phase 2 (Foundational) — wire types, dispatch, env. Verify `cargo check --workspace` passes and env vars reach a child shell.
3. Phase 3 (US1) — full Claude path: helper + ingress + classifier + adapter + statusline + setup script + OSC parser removal + lat.md sync.
4. **STOP and VALIDATE**: Run quickstart 1a–1d. Confirm AskUserQuestion succeeds, indicators update, no leakage.
5. Decision point: ship MVP (Claude only) or proceed to Phase 4 for Codex/Auggie parity.

### Incremental Delivery

1. Phase 1 + Phase 2 → foundation ready.
2. Phase 3 → **MVP**, quickstart 1 passes.
3. Phase 4 → quickstart 2 passes; Codex + Auggie back on parity (and now actually using the same channel, not silently no-oping).
4. Phase 6 → safety tests pin FR-025.
5. Phase 5 → verify extensibility.
6. Phase 7 → polish + final lat.md sync + manual quickstart sweep.

### Parallel Team Strategy

With multiple developers after Phase 2:

- **Developer A**: Phase 3 US1 (highest priority, MVP).
- **Developer B**: Phase 4 US2 (Codex + Auggie adapters — can start once helper signature is fixed by Phase 2).
- **Developer C**: Phase 6 US4 (safety tests — needs helper from US1 to land first; can prepare test scaffolding meanwhile).

US3 (Phase 5) waits until US1 and US2 complete because it verifies their combined surface.

---

## Notes

- The biggest diff in this feature is **deletions** (six dist/ shell scripts, AI hook OSC parsing in metadata.rs). Land deletions only after the new path replaces them, never before.
- No backward compatibility per [spec.md](./spec.md) Clarifications and FR-020 through FR-022. The old `printf > /dev/tty` hooks are gone the moment `setup-{provider}-hooks.sh` runs against an existing install.
- `lat check` must pass at end-of-phase for any phase that touches code. T027 (after US1) and T037 (after US2) are the within-phase gates; T050 is the final gate.
- Commit at each major milestone: end of Phase 2, end of Phase 3 (MVP), end of Phase 4, end of Phase 6, end of Phase 7.
- The server-restart approval gate (T055) is the only step requiring user interaction; everything else is autonomous.
- Avoid: writing to stdout or stderr from the helper or adapters; adding `#[allow]` lint suppressions; deleting OSC parsing for the pre-arm sentinel (FR-023); restarting `scribe-server` without explicit approval.
