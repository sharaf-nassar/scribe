---

description: "Task list — IME Composition and Preedit Handling"
---

# Tasks: IME Composition and Preedit Handling

**Input**: Design documents from `/specs/008-ime-composition/`
**Prerequisites**: `plan.md`, `spec.md` (loaded), `research.md`, `data-model.md`, `contracts/ime-pipeline.md`, `quickstart.md`

**Tests**: Automated tests are NOT requested in this feature's spec. Verification is **manual** via `quickstart.md`, with the existing 122-test input-pipeline regression suite gating the non-IME byte path (rationale: `research.md#R11` — IME requires a real OS input method to exercise meaningfully; the existing suite already pins the byte-identical contract).

**Organization**: Tasks are grouped by user story to enable independent implementation and verification.

**Constitution Gates**: Each story has manual independent-test criteria. Lat.md updates and `lat check` are mandatory polish-phase tasks. No server restarts. No new dependencies. No protocol/config schema changes.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no shared in-flight dependencies).
- **[Story]**: `[US1]` / `[US2]` / `[US3]` for story-scoped work; absent for Setup / Foundational / Polish.

## Path Conventions

This is a Rust workspace. All implementation paths are relative to repo root and live under `crates/scribe-client/src/` unless noted. The plan (`plan.md` § Project Structure) confines changes to that crate plus light renderer reuse.

---

## Phase 1: Setup

**Purpose**: Confirm baseline before touching code.

- [ ] T001 Verify branch `008-ime-composition` is checked out and the workspace builds clean: run `cargo build -p scribe-client` and confirm no warnings introduced by an in-flight WIP. No code changes.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Land the minimum scaffolding all three user stories depend on — the `WindowEvent::Ime` arm, the `ime_active` flag, and the activation gate predicate. After this checkpoint, US1 / US2 / US3 cannot truly be done in parallel (they share `main.rs` and `input.rs`), but they can be sequenced cleanly.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [ ] T002 Add `ime_active: bool` field to `App` in `crates/scribe-client/src/main.rs` (default `false`), constructed in `App::new`. No behavior change yet.
- [ ] T003 Add an empty `WindowEvent::Ime(_)` arm to `App`'s `ApplicationHandler::window_event` in `crates/scribe-client/src/main.rs` that pattern-matches on `Ime::{Enabled, Preedit, Commit, Disabled}` with `todo!()` bodies — establishes the dispatch site without behavior.
- [ ] T004 Implement `App::ime_should_be_allowed()` helper in `crates/scribe-client/src/main.rs` returning the activation-gate predicate (`window_focused && x11_focus_guard_says_active && current_focused_surface == TerminalPane`) per `data-model.md#ImeActivationGate`. Reuses existing `window_focused` / `x11_focus.rs` accessors; no parallel state.

**Checkpoint**: `WindowEvent::Ime` reaches `App`; gate predicate compiles and is callable. Ready for US1.

---

## Phase 3: User Story 1 — Compose and commit via OS IME (Priority: P1) 🎯 MVP

**Goal**: A CJK / dead-key / Compose user can type and commit text into a focused pane; non-IME typing is unchanged.

**Independent Test**: Run `quickstart.md` § P1 with any OS IME and confirm 你好 (or accented Latin via dead keys) lands at the shell prompt. Unfocused panes receive nothing.

### Implementation for User Story 1

- [ ] T005 [US1] Wire `Ime::Enabled` and `Ime::Disabled` handlers in the new `WindowEvent::Ime` arm in `crates/scribe-client/src/main.rs`: set/clear `app.ime_active`, mark redraw dirty. No preedit state yet.
- [ ] T006 [US1] Wire `Ime::Commit(text)` handler in the same arm in `crates/scribe-client/src/main.rs`: send `ClientMessage::KeyInput { session_id: focused_pane.session_id, bytes: text.into_bytes() }` to the existing IPC sender. Bypass the level-4 encoder. Per `contracts/ime-pipeline.md#Ime::Commit`.
- [ ] T007 [US1] Wire `Ime::Preedit` handler as a placeholder in `crates/scribe-client/src/main.rs` that records the latest preedit text on a temporary scratch field but does NOT render. US2 replaces this with `PreeditState`. (Rationale: keeps the arm exhaustive so future variants don't silently fall through.)
- [ ] T008 [US1] Call `window.set_ime_allowed(true)` from `App` whenever the activation gate transitions to allowed, and `set_ime_allowed(false)` on the reverse transition, in `crates/scribe-client/src/main.rs`. Trigger sites: `WindowEvent::Focused(true/false)`, focused-pane change, and the X11-focus-guard transition path.
- [ ] T009 [US1] Add an IME-active short-circuit at the entry of the keyboard-input dispatch in `crates/scribe-client/src/input.rs` (top of `translate_key` or its caller in `main.rs`): when `app.ime_active && key_was_consumed_by_ime` (winit `KeyEvent` semantics — see `research.md#R6`), return early before any encoder or shortcut layer. Do NOT modify `translate_key_kitty`, `translate_key`'s legacy path, or `translate_numpad_app_keypad`.
- [ ] T010 [US1] Manual verification: run `quickstart.md` § P1 on at least one platform; confirm CJK commit + dead-key composition + unfocused-pane no-op. Capture the platform(s) tested.

**Checkpoint**: P1 user story is fully functional. CJK / dead-key users can type. Non-IME paths byte-identical.

---

## Phase 4: User Story 2 — Inline preedit at cursor (Priority: P2)

**Goal**: Composing text renders at the cursor cell with a distinguishing visual treatment (underline by default).

**Independent Test**: Run `quickstart.md` § P2 — observe preedit visual during composition, confirm cancel and commit both clear cleanly with no one-frame residue, confirm terminal grid contents are never altered by preedit.

### Implementation for User Story 2

- [ ] T011 [US2] Create `crates/scribe-client/src/preedit.rs` defining `PreeditState { text: String, caret: Option<(usize, usize)>, start_row: usize, start_col: usize }` per `data-model.md#PreeditState`. Add module declaration in `crates/scribe-client/src/main.rs` or the existing module-root. Pure data; no behavior.
- [ ] T012 [US2] Add `preedit: Option<PreeditState>` field to `App` in `crates/scribe-client/src/main.rs` (default `None`). Replace the T007 scratch field. No render hookup yet.
- [ ] T013 [US2] Update the `Ime::Preedit(text, caret)` handler in `crates/scribe-client/src/main.rs` to create/update/drop `app.preedit` per the state machine in `data-model.md`. Empty text clears; non-empty text either creates `PreeditState` (capturing current cursor cell as `start_row` / `start_col`) or updates the existing one's text + caret.
- [ ] T014 [US2] Clear `app.preedit` inside the `Ime::Commit` handler in `crates/scribe-client/src/main.rs` BEFORE sending `KeyInput`, so the preedit cells visually disappear in the same frame as the committed PTY output arrives (per UX-002).
- [ ] T015 [US2] Add a per-frame preedit-overlay computation to `App`'s redraw path in `crates/scribe-client/src/main.rs` (or `preedit.rs`): from `app.preedit` + focused pane's current cursor cell, produce a `PreeditOverlay { glyphs, underline_quad, caret_quad }` consumed by the renderer.
- [ ] T016 [US2] Wire the preedit overlay into the renderer draw path: reuse `crates/scribe-renderer/src/chrome.rs#solid_quad` for the underline + optional caret-segment background; reuse the existing cosmic-text shaping + atlas path for the glyphs. Layer above the terminal grid, below search/dialog overlays. No new wgpu pipeline, no new shaders.
- [ ] T017 [US2] Manual verification: run `quickstart.md` § P2; confirm preedit visible during composition, cancel clears cleanly, terminal grid unchanged after cancel, commit replaces preedit with PTY-echo text in the same frame.

**Checkpoint**: P2 user story is fully functional. Inline preedit anchors composition at the cursor.

---

## Phase 5: User Story 3 — IME state survives workflow events (Priority: P3)

**Goal**: Pane switches, focus loss, scroll, resize, alt-screen redraws, and DPI changes never leave orphan preedit or stuck IME state.

**Independent Test**: Run `quickstart.md` § P3a–P3d on at least one platform; confirm no orphan cells, popup follows cursor on every cursor-cell movement.

### Implementation for User Story 3

- [ ] T018 [US3] Push `window.set_ime_cursor_area(position, size)` from `App`'s redraw path in `crates/scribe-client/src/main.rs` on every frame where the focused pane's cursor cell moved or the gate state changed. Cell rect is already computed for cursor rendering — read it from the existing accessor. Per `contracts/ime-pipeline.md#Window::set_ime_cursor_area`.
- [ ] T019 [US3] On `WindowEvent::Resized` and `WindowEvent::ScaleFactorChanged` in `crates/scribe-client/src/main.rs`, force an immediate cursor-area push after the existing resize handling.
- [ ] T020 [US3] On focused-pane change in `crates/scribe-client/src/main.rs` (the existing pane-focus event path), clear `app.preedit`, re-evaluate the activation gate, and push a fresh cursor-area for the newly focused pane.
- [ ] T021 [US3] On `WindowEvent::Focused(false)` and on the X11-focus-guard "inactive" transition in `crates/scribe-client/src/main.rs`, call `set_ime_allowed(false)` and clear `app.preedit`. Mirror `WindowEvent::Focused(true)` to re-enable per the gate.
- [ ] T022 [US3] Gate IME activation on the current focused UI surface: when the search overlay (`crates/scribe-client/src/search_overlay.rs`) or a modal dialog (`update_dialog.rs` / `close_dialog.rs` / `context_menu.rs`) is open, the gate predicate fails (FR-012). Wire the surface-state read into `App::ime_should_be_allowed()` from T004.
- [ ] T023 [US3] Suppress redraw-loop wake-ups for cursor-area updates while the window is occluded — reuse the existing occlusion gate pattern from `crates/scribe-client/src/ai_indicator.rs` per PR-002.
- [ ] T024 [US3] Manual verification: run `quickstart.md` § P3a (pane switch), § P3b (focus loss), § P3c (scroll / alt-screen), § P3d (resize / DPI). Confirm no orphan preedit and popup-follows-cursor across all transitions.

**Checkpoint**: All three user stories are functional and independently verified.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Sync docs with the new code, run the regression / audit / verification gates required by the constitution and CLAUDE.md.

- [ ] T025 [P] Update `lat.md/client.md` Input section: add a new subsection `IME Composition` describing the `WindowEvent::Ime` arm, the activation gate, the `PreeditState` data model, and the encoder-bypass for commits. Cross-reference the existing `Key Translation Priority` section to note IME's place above the level-4 encoder.
- [ ] T026 [P] Update `lat.md/rendering.md`: add a one-line note under the chrome rendering section pointing at the preedit overlay, with a back-reference to `client.md#IME Composition`.
- [ ] T027 Run `lat check` from repo root; resolve any wiki-link or code-ref failures. All references in T025–T026 must validate against `crates/scribe-client/src/main.rs` / `preedit.rs` / `input.rs`.
- [ ] T028 Run the regression baseline: `cargo test -p scribe-client --lib input` (target: full pre-existing test suite passes with zero new failures, per SC-003). Capture pass count in the PR description.
- [ ] T029 Run the full workspace build: `cargo build --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`. Both must be clean.
- [ ] T030 [P] Annotate `design/modern-terminal-audit-2026-05-18.md` with the same "Update — Shipped" callout pattern used for Kitty CSI-u, under the `IME composition / preedit handling (highest severity)` heading. Update the Summary postscript counts. Satisfies SC-006.
- [ ] T031 Final manual sweep against `quickstart.md` § Sign-off checklist on each platform the contributor has access to. Mark N/A for platforms unavailable. Paste the checklist into the PR description.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — can start immediately.
- **Foundational (Phase 2)**: Depends on Setup; BLOCKS all user stories.
- **User Stories (Phases 3–5)**: All depend on Foundational. They are listed in priority order (P1 → P2 → P3) and **share `main.rs` + `input.rs`**, so they execute sequentially in a single-developer flow. (A multi-developer team could split US3's surface-gate work — T022 — onto a parallel branch since it touches `search_overlay.rs` / dialog files, but the rest of US3 still serializes on `main.rs`.)
- **Polish (Phase 6)**: Depends on US1+US2+US3 being complete.

### User Story Dependencies

- **US1 (P1)**: Can start after Foundational. No dependencies on US2 or US3.
- **US2 (P2)**: Can start after Foundational. T007's scratch field is *replaced* by US2's `PreeditState`; if US2 happens before US1's T010 manual verification, no harm — US1's verification still works because T013's empty-text path is a superset of T007's no-render placeholder.
- **US3 (P3)**: Can start after US1+US2 — it depends on the `set_ime_allowed` / `set_ime_cursor_area` wiring from US1 and the `PreeditState` from US2 to have orphan-clearing behavior to validate.

### Within Each User Story

- No automated tests to write first.
- Foundational changes (struct fields, handler scaffolds) before behavior wiring.
- Behavior wiring before manual verification step.
- Manual verification step is the story's checkpoint.

### Parallel Opportunities

- Phase 1: single task; no parallelism.
- Phase 2: tasks are sequential additions to `main.rs`; no parallelism.
- Phase 3 (US1): all tasks touch `main.rs` / `input.rs`; sequential.
- Phase 4 (US2): all tasks touch `main.rs` / new `preedit.rs` / renderer; sequential.
- Phase 5 (US3): T022 (surface gate) can run in parallel with T018–T021 if work is split across developers; otherwise sequential.
- Phase 6: T025, T026, T030 are independent doc edits ([P] marked). T027 (`lat check`) gates on T025+T026. T028+T029 are independent verifications and can run in parallel.

---

## Parallel Example: Phase 6 (Polish)

```bash
# Three independent doc edits + two independent verification runs in parallel:
Task: "Update lat.md/client.md with IME Composition subsection (T025)"
Task: "Update lat.md/rendering.md preedit cross-reference (T026)"
Task: "Annotate design/modern-terminal-audit-2026-05-18.md with shipped IME note (T030)"
Task: "Run cargo test -p scribe-client --lib input (T028)"
Task: "Run cargo build --workspace + cargo clippy (T029)"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup (T001).
2. Complete Phase 2: Foundational (T002–T004).
3. Complete Phase 3: US1 (T005–T010).
4. **STOP and VALIDATE**: Run `quickstart.md` § P1 + § R1 (ASCII unchanged) + § R3 (cargo test).
5. Deploy/demo if ready — CJK users can already type at this point.

### Incremental Delivery

1. Foundation (Phase 1+2) → minimum scaffolding ready.
2. US1 → CJK / dead-key users can type (MVP).
3. US2 → preedit anchored inline at cursor.
4. US3 → robust under pane switches, focus loss, resize, alt-screen.
5. Polish → lat.md sync, audit-doc annotation, regression baselines green.

### Parallel Team Strategy

This feature does not parallelize well across developers — US1, US2, US3 mostly touch the same two files (`main.rs`, `input.rs`). With one developer, sequential P1 → P2 → P3 is the right shape. A second developer can take T022 (surface-gate / search-overlay / dialogs) and T030 (audit doc annotation) on a side branch once US1 is on `main`.

---

## Notes

- No new automated tests created; the existing input-pipeline test suite covers the byte-identical guarantee for non-IME keys. Future work (synthetic `WindowEvent::Ime` handler tests) is documented in `research.md#R11`.
- Every task gives an exact file path.
- Commit after each task or each story's checkpoint.
- `lat check` MUST pass before any PR is opened (CLAUDE.md post-task checklist).
- The audit-doc annotation in T030 closes SC-006 and matches the Kitty CSI-u pattern from 2026-05-21.
- Constitution principle II is satisfied via the manual quickstart paths in `quickstart.md` (one per story), plus the regression cargo-test run in T028.
