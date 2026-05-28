---
description: "Task list for Paste Confirmation (Multiline / Control-Character)"
---

# Tasks: Paste Confirmation (Multiline / Control-Character)

**Input**: Design documents from `/specs/011-paste-confirmation/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/paste-confirmation.md, quickstart.md

**Tests**: NOT included. Per QR-002 / Constitution II and the project's
test-only-on-explicit-request rule, verification is manual quickstart per user
story. The pure `classify_paste` function is the natural unit-test seam if
automated coverage is later requested.

**Organization**: Tasks are grouped by user story (P1 → P2 → P3) so each can be
implemented, tested, and delivered as an independent increment.

**Constitution Gates**: Tasks preserve crate boundaries (client / common /
settings), reuse the existing dialog + paste + config abstractions, add no new
dependency or protocol change, keep the disabled-default zero-cost path, and
include `lat.md` sync + `lat check`. No live Scribe server restart is required
or permitted without explicit user approval.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on incomplete tasks)
- **[Story]**: US1 / US2 / US3 (user-story phases only)
- Exact file paths are included in each task

## Path Conventions

Rust Cargo workspace (per plan.md). Relevant crates:
`crates/scribe-client/src/`, `crates/scribe-common/src/`,
`crates/scribe-settings/src/`. Knowledge graph: `lat.md/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: De-risk the gate placement and scaffold the new module.

- [x] T001 [P] Verify the non-paste `send_paste_message` call sites (~`crates/scribe-client/src/main.rs:9473` and ~`:10053`) are drag-and-drop / intentionally-ungated insertions, NOT in-scope clipboard/selection pastes (research R1). Record the finding; only if one is a genuine in-scope paste, note it must also route through `send_paste_data`.
- [x] T002 Create the new module `crates/scribe-client/src/paste_confirmation_dialog.rs` (empty skeleton) and declare `mod paste_confirmation_dialog;` in `crates/scribe-client/src/main.rs`.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The config field + pure helpers every user story depends on.

**⚠️ CRITICAL**: No user-story work can begin until this phase is complete.

- [x] T003 [P] Add `#[serde(default)] pub paste_confirmation: bool` to `TerminalConfig` (after `clipboard_policy`) and `paste_confirmation: false` to `impl Default for TerminalConfig`, in `crates/scribe-common/src/config.rs` (research R6/R7 — defaults false, backward compatible).
- [x] T004 [P] Implement `PasteRisk { has_line_break: bool, has_control: bool }` and the pure `classify_paste(text: &str) -> Option<PasteRisk>` in `crates/scribe-client/src/paste_confirmation_dialog.rs` — `'\n'|'\r'` → line break, `'\t'` ignored, any other `char::is_control()` → control; `Some` iff either is set (research R3).
- [x] T005 Implement the caret-escape preview helper in `crates/scribe-client/src/paste_confirmation_dialog.rs` — control bytes → caret notation (`ESC`→`^[`, `CR`→`^M`, NUL→`^@`, DEL→`^?`, C1→`\u{NN}`), tabs→spaces; per-line truncation to `MAX_PREVIEW_COLS = 56` (reuse the `truncate_for_display` head/tail pattern) and ≤ `MAX_PREVIEW_LINES = 8` with a `… (+N more lines)` summary; never emit a raw control byte (research R4 / FR-005 / SC-008).

**Checkpoint**: Config key exists and defaults false; classifier + preview helper unit-inspectable.

---

## Phase 3: User Story 1 - Catch an accidental multi-line paste before it runs (Priority: P1) 🎯 MVP

**Goal**: With the setting on and bracketed paste off, a multi-line paste via the keybinding or context-menu pops a Cancel-default confirmation showing the line count + preview before any byte reaches the PTY; Paste delivers byte-identically, Cancel sends nothing.

**Independent Test**: Set `terminal.paste_confirmation = true` in config; in an unbracketed app (e.g. bare `cat`), paste a 3-line block → dialog appears with reason + preview, Cancel sends 0 bytes, Paste sends all 3 lines exactly. Bracketed prompt → no dialog (quickstart US1).

- [x] T006 [US1] Define `PasteConfirmationDialog` (fields: `content: String`, `target: PasteTarget`, `risk: PasteRisk`, `focused: ButtonIndex`, `hovered: Option<usize>`, `button_rects: [Rect; 2]`) and `PasteConfirmationAction { Paste, Cancel }` in `crates/scribe-client/src/paste_confirmation_dialog.rs`, cloning the `DisallowedSchemeDialog` shape with **Cancel = index 0 = default focus** (research R5).
- [x] T007 [US1] Implement the dialog rendering + interaction in `crates/scribe-client/src/paste_confirmation_dialog.rs`: `new(content, target, risk)`, `title()`, `body_lines()` (reason line from `risk` + caret-escaped preview via the T005 helper), `build_instances(ctx)`, `build_buttons`, `focus_next/prev`, `confirm`, `update_hover`, `click` — mirroring `disallowed_scheme_dialog.rs`.
- [x] T008 [US1] Add `paste_confirmation_dialog: Option<paste_confirmation_dialog::PasteConfirmationDialog>` to `App` (init `None`) and add its `build_instances` call in the dialog render path (sibling of the disallowed-scheme dialog, ~`crates/scribe-client/src/main.rs:5395`).
- [x] T009 [US1] Add the window-event guard `if self.paste_confirmation_dialog.is_some() { self.handle_paste_confirmation_dialog_window_event(...); return true; }` among the existing dialog guards (~`crates/scribe-client/src/main.rs:1723`, preserving one-modal precedence) and implement `handle_paste_confirmation_dialog_window_event` (Esc→Cancel, Enter→focused, Tab/Shift+Tab cycle, hover/click) in `crates/scribe-client/src/main.rs`.
- [x] T010 [US1] Implement the gate in `App::send_paste_data` (`crates/scribe-client/src/main.rs:6988`): after `prepare_paste_target()` returns `target`, if `self.config.terminal.paste_confirmation && !target.bracketed` and `classify_paste(text).is_some()`, store `(content, target, risk)` in `paste_confirmation_dialog`, `request_redraw()`, and `return` before the send tail (research R1).
- [x] T011 [US1] Implement `App::handle_paste_confirmation_action` in `crates/scribe-client/src/main.rs`: on `Paste`, `take()` the dialog and resume via `Self::try_send_single_paste` else `Self::send_chunked_paste` with the parked `target` (bypassing the gate); on `Cancel`, drop and send nothing; wire it from the dialog event handler (research R2). On `Paste`, if the parked `target` session is no longer live, drop the paste safely — no crash, no delivery to a different pane (covers the "pane closed while the dialog is open" edge case). The parked decision is honored even if the setting was toggled off while the dialog was open (the gate is only consulted at request time).

**Checkpoint**: US1 fully functional for keybinding + context-menu paste (both already route through `send_paste_data`). MVP — enable via manual config edit and demo.

---

## Phase 4: User Story 2 - Catch hidden control/escape characters in a paste (Priority: P2)

**Goal**: A single-line (or multi-line) paste containing non-tab control/escape bytes triggers the same confirmation, with those bytes shown in caret notation and named in the reason line.

**Independent Test**: With the setting on, paste a single-line string containing an embedded `ESC` into an unbracketed app → dialog appears (no line break needed), reason names control characters, preview shows `^[`; a tabs-only single line does NOT trigger (quickstart US2).

- [x] T012 [US2] In `body_lines()` (`crates/scribe-client/src/paste_confirmation_dialog.rs`), make the reason line distinguish the three cases — multiline-only, control-only, and both (e.g. "12 lines", "contains control characters", "12 lines · 3 control characters") — and confirm a single-line control-only paste yields `classify_paste(...) == Some` so the gate fires and the caret-escaped preview renders. (Detection + caret rendering already exist from Phase 2/US1; this completes the control-only presentation.)

**Checkpoint**: US1 and US2 both work independently.

---

## Phase 5: User Story 3 - Discoverable, live, and uniform across paste sources (Priority: P3)

**Goal**: The Terminal settings page exposes the off-by-default toggle with plain-language helper text; it live-reloads; and the gate applies uniformly to keybinding, context-menu, AND middle-click primary-selection paste.

**Independent Test**: Settings → Terminal shows the toggle off by default; turning it on takes effect on the next paste with no restart; with it on, risky pastes via keybinding, right-click Paste, and middle-click all confirm; a >4 KiB middle-click paste delivers fully on confirm (quickstart US3).

- [x] T013 [US3] Refactor `App::perform_primary_paste` (`crates/scribe-client/src/main.rs:9164`) to fetch the primary-selection text and call `self.send_paste_data(&text)`, removing its inline bracketed-wrap + single-`KeyInput` send. This routes middle-click through the gate and (per research R1) fixes the latent >4 KiB un-chunked primary-paste bug.
- [x] T014 [P] [US3] Add the Terminal-page toggle block in `crates/scribe-settings/src/assets/settings.html`: `<div class="toggle off" data-key="terminal.paste_confirmation">` with a clear label ("Confirm risky pastes") and helper text that explains it warns before pasting multi-line / control-character text into apps that would run it immediately, and that it defers to bracketed-paste-aware apps.
- [x] T015 [P] [US3] Add `setToggleValue("terminal.paste_confirmation", config.terminal?.paste_confirmation ?? false)` to the Terminal section of `loadConfig` in `crates/scribe-settings/src/assets/settings.js` (click dispatch is handled by the generic toggle listener).
- [x] T016 [P] [US3] Add `"terminal.paste_confirmation"` to the `apply_terminal_key` match arm and assign `config.terminal.paste_confirmation = value.as_bool()...` in `apply_terminal_behavior_key`, in `crates/scribe-settings/src/apply.rs`.
- [x] T017 [US3] Verify live reload end-to-end: the gate reads `self.config.terminal.paste_confirmation` and `ConfigReloaded` already refreshes `App.config`, so toggling takes effect on the next paste with no restart. Add code only if a gap is found (research R6 expects none).

**Checkpoint**: All three user stories independently functional.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Knowledge-graph sync, build/lint, and end-to-end verification.

- [x] T018 [P] Update `lat.md/client.md`: add a "Paste Confirmation Dialog" subsection under Dialogs and a paste-gate note under Input, with `[[crates/scribe-client/src/paste_confirmation_dialog.rs#PasteConfirmationDialog]]`, `[[crates/scribe-client/src/paste_confirmation_dialog.rs#classify_paste]]`, and `[[crates/scribe-client/src/main.rs#App#send_paste_data]]` refs.
- [x] T019 [P] Update `lat.md/common.md`: add `paste_confirmation` to the `TerminalConfig` sentence under Configuration#Terminal.
- [x] T020 [P] Update `lat.md/settings.md`: add the paste-confirmation toggle to Config Application#Terminal Keys and reference `[[crates/scribe-settings/src/apply.rs#apply_terminal_behavior_key]]`.
- [x] T021 Run `lat check` — all wiki links and code refs MUST pass; fix any breakage introduced by T018–T020 (and confirm the new `[[crates/...]]` refs resolve).
- [x] T022 Build + lint: `cargo build` and `cargo clippy` clean for `scribe-client`, `scribe-common`, and `scribe-settings`.
- [x] T023 Execute `quickstart.md`: US1, US2, US3 scenarios + the edge cases (pane closed while the dialog is open → paste dropped safely; setting toggled off mid-dialog → in-flight decision honored) + the out-of-scope checks (drag-and-drop / copy-on-select / OSC 52 unaffected) + the performance checks. **Performance method**: the disabled-path zero-cost claim (SC-005 / PR-001) is confirmed *by inspection* — the gate short-circuits on `!self.config.terminal.paste_confirmation` before `classify_paste` runs, so a disabled paste takes the exact prior code path; the large-paste responsiveness and no-parked-content-leak claims are confirmed by manual observation with a multi-megabyte paste. Record results in the completion report.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies.
- **Foundational (Phase 2)**: after Setup — BLOCKS all user stories.
- **US1 (Phase 3)**: after Foundational. MVP.
- **US2 (Phase 4)**: after US1 (reuses the US1 dialog + Phase-2 classifier/caret).
- **US3 (Phase 5)**: settings tasks (T014–T016) need only T003; T013 needs the US1 gate. Sequenced after US1 for coherent incremental delivery and because its manual test exercises the gate.
- **Polish (Phase 6)**: after the desired stories; includes `lat check` + build/lint + quickstart.

### User Story Dependencies

- **US1 (P1)**: independent once Foundational is done. No dependency on US2/US3.
- **US2 (P2)**: builds on US1's dialog; independently testable (single-line control paste).
- **US3 (P3)**: builds on US1's gate; independently testable (settings toggle + middle-click + live reload).

### Within Each Story

- No tests to write first (none requested). Module/struct before wiring; gate before resume; settings field (T003) before settings UI (T014–T016).

### Parallel Opportunities

- **Foundational**: T003 (config.rs) ‖ T004 (new module). T005 follows T004 (same file).
- **US3**: T013 (main.rs) ‖ T014 (settings.html) ‖ T015 (settings.js) ‖ T016 (apply.rs) — four different files.
- **Polish**: T018 ‖ T019 ‖ T020 (three different `lat.md` files); T021 (`lat check`) after all three.

---

## Parallel Example: Phase 5 (US3)

```bash
# Four different files, no interdependencies — safe to run together:
Task: "T013 refactor perform_primary_paste to call send_paste_data in crates/scribe-client/src/main.rs"
Task: "T014 add Terminal toggle in crates/scribe-settings/src/assets/settings.html"
Task: "T015 add setToggleValue in crates/scribe-settings/src/assets/settings.js"
Task: "T016 add terminal.paste_confirmation to apply.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 1 Setup → Phase 2 Foundational → Phase 3 US1.
2. Enable via `terminal.paste_confirmation = true` in config.
3. **STOP and VALIDATE**: run the US1 quickstart (multiline gate, Cancel/Paste, bracketed deferral, single-line passthrough).
4. Demo the MVP.

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. US1 → multiline confirmation (keybinding + context menu) — MVP.
3. US2 → control/escape coverage + caret preview.
4. US3 → settings discoverability + live reload + middle-click uniformity (+ >4 KiB chunking fix).
5. Polish → `lat.md` sync, `lat check`, build/lint, quickstart.

---

## Notes

- [P] = different files, no dependency on incomplete tasks.
- No automated tests are added (QR-002 / test-only-on-request). `classify_paste`
  is the unit-test seam if tests are requested later.
- Byte-identical delivery on confirm is an invariant (contract C5 / SC-002) —
  the gate must never transform pasted content; the preview is display-only.
- Terminology: "control/escape byte" (spec), "control byte", and "control
  character" all denote the same FR-002 set — `char::is_control()` minus
  `\t`/`\n`/`\r` (C0 except tab/LF/CR, DEL, C1). Pinned in research R3 / contract C4.
- Keep `lat.md` in sync (Phase 6) and pass `lat check` before reporting done
  (CLAUDE.md post-task checklist + Constitution operational safety).
- Do NOT restart the Scribe server to verify; the client can be re-run without
  a server restart.
- Commit after each task or logical group.
