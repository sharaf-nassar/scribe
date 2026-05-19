---
description: "Task list for Keyboard Protocol & Command Awareness"
---

# Tasks: Keyboard Protocol & Command Awareness

**Input**: Design documents from `/specs/005-keyboard-command-awareness/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: No automated test tasks. Spec QR-002 and the project's test-on-explicit-request
rule mean verification is manual quickstart (Constitution II compliant). The Kitty
conformance matrix is a recommended *future* automated suite (extends the existing
`scribe-test` input-simulation harness) requiring explicit approval — intentionally NOT
tasked here.

**Organization**: Grouped by user story. All work is client-side; no IPC/protocol/persistence
change (research-verified). No live Scribe server restart required.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no incomplete dependency)
- **[Story]**: US1 / US2 / US3 (Setup, Foundational, Polish carry no story label)
- Exact file paths included.

## Path Conventions

Existing Rust workspace. Production paths: `crates/scribe-client/src/`,
`crates/scribe-common/src/`, `crates/scribe-settings/src/assets/`, `lat.md/`. No new
crates/modules.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Establish the regression baseline and confirm a clean starting point. No
functional change.

- [ ] T001 [P] **[USER-OWNED — requires running GPU client; cannot run headless here, must not restart server]** Capture the legacy-encoding byte baseline: in a non-negotiating shell, record `scribe-client` PTY input bytes for a broad keymap (printable, Ctrl/Alt/Shift combos, named keys, Esc, Tab) and save the reference under `specs/005-keyboard-command-awareness/baseline-legacy-input.txt` for the SC-003 non-regression comparison
- [X] T002 [P] Re-verify alacritty_terminal 0.26.0-rc1 Kitty keyboard support against the pinned crate source: **(a)** flag enablement (upstream issue #8836), and **(b)** push/pop stack behavior — confirm `CSI > <flags> u` push and `CSI < <n> u` pop drive `Term::mode()` to the correct top-of-stack (covers spec FR-003 push/pop nesting; end-to-end verified in T017 §4). If push/pop is unreliable, record a fallback decision (e.g. encode against the outermost negotiated flag set, or document the limitation) — append all findings to the Resolved Risks list in `specs/005-keyboard-command-awareness/research.md`
- [X] T003 [P] Confirm clean baseline: `cargo build -p scribe-client -p scribe-common` → exit 0, clean (no code change in this task)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Cross-story blockers.

**None.** Research proved US1 (keyboard) and US2 (command-awareness) share no code path and
require no IPC/protocol change, so there is no global blocking phase. The command-awareness
shared types (`CommandStatus`/`CommandRecord`/`UiEvent.exit_code`) are created in US2 (the
earliest consumer; US3 builds on them). US1 and US2 may begin immediately and in parallel
after Setup.

**Checkpoint**: Setup done → US1 and US2 can both start (parallel-safe; see Parallel notes
re: shared `main.rs`).

---

## Phase 3: User Story 1 - Protocol-aware apps receive unambiguous keys (Priority: P1) 🎯 MVP

**Goal**: When an application negotiates the Kitty keyboard protocol, encode every
key/modifier combination (plus key-repeat/release) as protocol-conformant CSI-u honoring
exactly the negotiated flags; legacy encoding stays byte-identical otherwise.

**Independent Test**: quickstart.md US1 — byte-probe a negotiating app across the
key/modifier matrix and event types; confirm legacy byte-identity with nothing negotiated.

### Implementation for User Story 1

- [X] T004 [P] [US1] Add `keyboard_protocol_enhanced: bool` to `TerminalConfig` (`#[serde(default = "default_true")]`, default `true`) and thread through the config-reload/apply path in `crates/scribe-common/src/config.rs`
- [X] T005 [US1] Replace the `KeyboardProtocol` enum with a `KittyFlags` struct (5 bool fields: disambiguate, report_event_types, report_alternate_keys, report_all_keys, report_associated_text) plus `is_kitty()`/legacy helpers in `crates/scribe-client/src/input.rs`
- [X] T006 [US1] Add a `NamedKey → u32` functional-key codepoint table in `crates/scribe-client/src/input.rs` covering **(a)** non-modifier named keys (Esc, Enter, Tab, Backspace, arrows, Home/End/Ins/Del, PgUp/PgDn, F1–F20) and **(b)** modifier & lock keys per the Kitty protocol — left/right Shift, Control, Alt, Super, plus CapsLock/NumLock — using winit `KeyEvent.location` to disambiguate left vs right, so modifier-only presses encode under `report_all_keys`/`report_event_types` instead of being swallowed. Closes spec Edge Case "Modifier-only press … must encode per the protocol, not be swallowed"; required for SC-001 (100% of defined key+modifier combinations) (depends on T005)
- [X] T007 [US1] Extend `build_csi_u_seq` to accept `event_type: Option<u8>` and emit both the 2-param and 3-param CSI-u forms in `crates/scribe-client/src/input.rs` (depends on T005)
- [X] T008 [US1] Thread `KittyFlags` through `translate_key_action`/`translate_key`/`translate_named_with_modifiers`/`translate_named_special` and emit conformant CSI-u for ALL named keys (not just Enter) per `disambiguate`/`report_all_keys` in `crates/scribe-client/src/input.rs` (depends on T005, T006, T007)
- [X] T009 [US1] Thread `KittyFlags` into `translate_character_with_modifiers` so character keys emit CSI-u when flags require (e.g. `Ctrl+I` no longer collapses to `Tab`) in `crates/scribe-client/src/input.rs` (depends on T008)
- [X] T010 [US1] Add the alternate-key field (base via `event.key_without_modifiers()`, shifted via `logical_key`) when `report_alternate_keys`, and the associated-text field (`event.text`) when `report_associated_text` in `crates/scribe-client/src/input.rs` (depends on T008)
- [X] T011 [US1] Add an explicit all-false-`KittyFlags` legacy guard so every legacy path is byte-identical to the T001 baseline (SC-003) in `crates/scribe-client/src/input.rs` (depends on T008, T009, T010)
- [X] T012 [US1] Rewrite `focused_keyboard_protocol()` to return `KittyFlags` read from the focused pane `Term::mode()` (all 5 bits), forced all-false when `keyboard_protocol_enhanced` is off, in `crates/scribe-client/src/main.rs` (depends on T004, T005)
- [X] T013 [US1] Relax the `ElementState::Pressed` gate on the terminal-key path ONLY when `report_event_types` (press=1, `event.repeat`→2, `Released`→3); keep shortcut/overlay/blink checks `Pressed`-only in `crates/scribe-client/src/main.rs` (depends on T012, T007)
- [X] T014 [US1] Verify and preserve the Codex `Alt+Enter` override (fires before generic Kitty encoding; no double-encode) and reconcile it with the new encoder output for that key in `crates/scribe-client/src/main.rs` (depends on T013)
- [X] T015 [P] [US1] Surface the `keyboard_protocol_enhanced` toggle on the Settings webview Terminal page in `crates/scribe-settings/src/assets/` (depends on T004)
- [X] T016 [US1] Update `lat.md/client.md` "Key Translation Priority" to the five-flag model + event-type forwarding, then run `lat check` (depends on T011, T013)
- [ ] T017 [US1] Manual verification: run quickstart.md US1 scenarios 1–7 (disambiguate, event types, subset, push/pop, legacy non-regression vs T001 baseline, multi-pane isolation, paste/dead-key/Codex non-regression) (depends on T014, T015, T016)

**Checkpoint**: US1 fully functional and independently testable (MVP).

---

## Phase 4: User Story 2 - Failed commands are immediately visible (Priority: P1)

**Goal**: Stop dropping the OSC 133 `exit_code` at the client; classify each command
Success/Failure/Unknown; differentiate scrollbar marks by status and show the latest
outcome in the status bar, never showing unknown as failure.

**Independent Test**: quickstart.md US2 — run a passing then failing command; confirm
distinct, no-scroll/no-hover differentiation, unknown≠failure, trim alignment, per-pane,
reattach non-misleading.

### Implementation for User Story 2

- [X] T018 [US2] Add `exit_code: Option<i32>` to `UiEvent::PromptMark` and forward it (stop dropping it) in the metadata dispatch arm in `crates/scribe-client/src/ipc_client.rs`
- [X] T019 [US2] Add `enum CommandStatus { Success, Failure, Unknown }` and `struct CommandRecord { abs_pos: usize, status: CommandStatus }` in `crates/scribe-client/src/pane.rs`
- [X] T020 [US2] Replace `Pane::prompt_marks: Vec<usize>` with `command_records: Vec<CommandRecord>` (update `Pane::new` init) and add `Pane::last_command_status: Option<CommandStatus>` in `crates/scribe-client/src/pane.rs` (depends on T019)
- [X] T021 [US2] Update `shift_absolute_marks_after_trim` to shift `record.abs_pos` and drop trimmed records (signature change), and fix all call sites in `crates/scribe-client/src/pane.rs` and `crates/scribe-client/src/main.rs` (depends on T020)
- [X] T022 [US2] Implement the `handle_prompt_mark` state machine in `crates/scribe-client/src/main.rs`: `A`→push `Unknown`; `D`→resolve the open record (0→Success, ≠0→Failure, `None`→stays Unknown); `B`/`C` unchanged; `D` with no open record ignored; update `last_command_status` (depends on T018, T020)
- [X] T023 [US2] Color scrollbar ticks by `CommandStatus` (Unknown = existing neutral; Success/Failure visually distinct) using **theme-derived** palette entries (per the FR-008 decision — colors follow the active theme so high-contrast/accessible themes apply; scrollbar color is a redundant secondary cue, the status-bar glyph in T024 is authoritative) iterating `command_records` in `crates/scribe-client/src/scrollbar.rs` (depends on T020)
- [X] T024 [US2] Add `last_command_status` to `StatusBarData` and render a left-segment indicator (✓ / ✗ / neutral `?`) following the existing `connected_dot` pattern in `crates/scribe-client/src/status_bar.rs`, fed from the focused pane in `crates/scribe-client/src/main.rs` (depends on T020, T022)
- [X] T025 [US2] Update `lat.md/client.md` "Scrollbar#Prompt Mark Indicators" and "IPC Client", and the `lat.md/pty.md` OSC 133 section cross-reference (exit status now surfaced client-side), then run `lat check` (depends on T022, T023, T024)
- [ ] T026 [US2] **[USER-OWNED — manual quickstart, needs GPU client]** Manual verification: run quickstart.md US2 scenarios 1–6 (pass/fail distinct without scroll/hover, status-bar outcome, long-running/unknown ≠ failure, trim alignment 0-drift, per-pane isolation, reattach + cold-restart non-misleading) (depends on T024, T025)

**Checkpoint**: US1 AND US2 both independently functional (both P1 — either is a shippable MVP slice).

---

## Phase 5: User Story 3 - Jump to commands and failures (Priority: P2)

**Goal**: Keyboard navigation between command boundaries plus a one-action jump to the most
recent failed command, with a non-disruptive signal when none exists.

**Independent Test**: quickstart.md US3 — in a seeded scrollback with ≥1 failure, navigate
boundaries and jump-to-failure with the keyboard only; viewport lands correctly every time.

**Dependency**: requires US2 `command_records` (T020).

### Implementation for User Story 3

- [X] T027 [US3] Update `handle_prompt_jump_up`/`handle_prompt_jump_down` to iterate `command_records` by `abs_pos` (user-visible behavior unchanged) in `crates/scribe-client/src/main.rs` (depends on T020)
- [X] T028 [P] [US3] Add `jump_to_failure: KeyComboList` to `KeybindingsConfig` with a distinct `default_jump_to_failure()` and update the `Default` impl in `crates/scribe-common/src/config.rs`
- [X] T029 [US3] Implement `handle_jump_to_failure` (reverse-scan `command_records` for the most recent `Failure`; non-disruptive no-op when none) and wire the `KeyAction::JumpToFailure` dispatch in `crates/scribe-client/src/main.rs` (depends on T027, T028)
- [X] T030 [P] [US3] Surface the `jump_to_failure` binding on the Settings webview Keybindings page in `crates/scribe-settings/src/assets/` (depends on T028)
- [ ] T031 [US3] **[USER-OWNED — manual quickstart, needs GPU client]** Manual verification: run quickstart.md US3 scenarios 1–4 (boundary nav keyboard-only, jump-to-failure one action, sensible end-bound, no-failure non-disruptive signal) (depends on T029, T030)

**Checkpoint**: All three user stories independently functional.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [ ] T032 [P] Performance verification (PR-001): scripted keystroke-flood input-latency check in a Kitty-negotiating full-screen app, and scroll frame-rate at the 10,000-line scrollback cap with many command records; record exact commands + observations for the completion report (depends on T017, T026)
- [ ] T033 [P] Cross-platform spot-check on macOS for representative US1/US2/US3 scenarios (depends on T017, T026, T031)
- [X] T034 [P] Update `README.md` — add `jump_to_failure` to the keyboard-shortcuts table and `keyboard_protocol_enhanced` to the configuration section (depends on T004, T028)
- [X] T035 Final `lat.md` sync sweep + `lat check` all-green; assemble the completion report naming the verification commands run and residual risk (alacritty #8836 status, reattach-reset behavior) (depends on T032, T033, T034)

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies; T001–T003 all `[P]`.
- **Foundational (Phase 2)**: none (no global blockers).
- **US1 (Phase 3)** and **US2 (Phase 4)**: independent tracks, both start after Setup, can run **in parallel**.
- **US3 (Phase 5)**: depends on US2 `command_records` (T020); T028 may start anytime.
- **Polish (Phase 6)**: after the targeted stories complete; includes constitution/perf verification.

### User Story Dependencies

- **US1 (P1)**: independent. MVP-capable alone.
- **US2 (P1)**: independent of US1. MVP-capable alone.
- **US3 (P2)**: depends on US2 (T027 needs T020). Independently *testable* once US2 done.

### Within-Story Order

- US1 `input.rs` chain is sequential (same file): T005 → T006/T007 → T008 → T009/T010 → T011. `main.rs`: T012 (after T004, T005) → T013 → T014.
- US2: T019 → T020 → T021; T022 after T018+T020; T023 after T020; T024 after T020+T022; T025 after T022/23/24.
- US3: T027 after T020; T029 after T027+T028.

### Parallel Opportunities

- Setup: T001, T002, T003 together.
- Cross-track: entire US1 and US2 run in parallel by different developers. **Caution**: both edit `crates/scribe-client/src/main.rs` (US1: `focused_keyboard_protocol`/`handle_keyboard`; US2: `handle_prompt_mark`/status feed) — distinct functions; coordinate or serialize the `main.rs` merges.
- `[P]` within stories only across distinct files: T004 (config) ∥ T005 (input.rs); T015/T030 (settings assets) ∥ core; T028 (config) ∥ US3 main.rs; Polish T032/T033/T034.

### Parallel Example (cross-track, after Setup)

```bash
# Developer A — US1 (input.rs / main.rs / config.rs)
Task: "T005 Replace KeyboardProtocol with KittyFlags in crates/scribe-client/src/input.rs"
# Developer B — US2 (ipc_client.rs / pane.rs / scrollbar.rs / status_bar.rs)
Task: "T019 Add CommandStatus/CommandRecord in crates/scribe-client/src/pane.rs"
```

---

## Implementation Strategy

### MVP First

Minimal MVP = **Setup + US1** (the most continuously-hit gap per spec). US2 is an equally
valid standalone P1 slice — pick US1 or US2 as the first shippable increment, validate
against its quickstart section, then proceed.

### Incremental Delivery

1. Setup → baseline captured.
2. US1 → verify quickstart US1 → ship (MVP). *(or US2 first — both P1, independent.)*
3. US2 → verify quickstart US2 → ship.
4. US3 → verify quickstart US3 → ship.
5. Polish → performance + cross-platform + docs + final `lat check` + completion report.

### Parallel Team Strategy

After Setup: Dev A takes US1, Dev B takes US2 (coordinate `main.rs`), then either picks up
US3 once US2's T020 lands. Each story integrates and verifies independently.

---

## Notes

- No automated test tasks (QR-002 + project test-on-request rule); every story has a manual
  quickstart verification task instead (Constitution II compliant).
- `[P]` = different files, no incomplete dependency. `[Story]` maps to spec.md user stories.
- No IPC/protocol/persistence change; no live server restart. Config change is additive +
  defaulted (no migration).
- Update `lat.md` whenever behavior/architecture changes and keep `lat check` green
  (T016, T025, T035).
- Commit after each task or logical group. Stop at any checkpoint to validate independently.
