---

description: "Task list for Add Note From Hover Preview"
---

# Tasks: Add Note From Hover Preview

**Input**: Design documents from `/specs/007-add-note-from-hover/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/README.md](./contracts/README.md), [quickstart.md](./quickstart.md)

**Tests**: NOT requested by the spec or accepted plan. Verification is via manual quickstart per QR-002 and constitution principle II. No `tests/...` tasks below.

**Organization**: Tasks are grouped by user story (US1, US2, US3) for independent verification. Foundational phase establishes the shared types and keymap that all three stories build on.

**Constitution Gates**: All tasks preserve crate boundaries, reuse existing protocol unchanged, state measurable performance budgets via existing instrumentation, include a manual verification path, and do not require restarting the live Scribe server.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files / non-overlapping regions, no dependencies on incomplete tasks)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)
- Each task names the exact file path and (where useful) the line number identified in the plan/research

## Path Conventions

This is a Rust workspace with multiple crates. All source-code changes live in `crates/scribe-client/src/`. `lat.md/` documents the architecture and is updated when behavior changes.

---

## Phase 1: Setup

**Purpose**: Establish a clean baseline before any changes land.

- [X] T001 Confirm clean build baseline by running `cargo build -p scribe-client -p scribe-server` from the repo root; capture the output so any new compile errors introduced by subsequent tasks are clearly attributable.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Establish the shared keymap, shared editor-state type, preview-interaction extensions, and flush plumbing that all user-story work builds on. These are independently shippable as a single early commit (per the plan's "smallest, can ship first as a discrete commit" note), but they MUST land before US1/US2/US3 implementation begins.

**CRITICAL**: No user-story work in Phase 3+ can begin until this phase is complete.

- [X] T002 Flip the modal-editor commit keymap (FR-017) in `crates/scribe-client/src/main.rs` inside `handle_workspace_notes_keyboard` (around line 9701) — swap the bodies of the two Enter match arms so `Key::Named(NamedKey::Enter) if self.modifiers.control_key()` inserts `'\n'` via `push_char('\n')`, and the modifier-free `Key::Named(NamedKey::Enter)` arm calls `save_workspace_notes_modal()`. Update the section comment if one exists.
- [X] T003 [P] Fix the modal spacebar bug (FR-018) by adding a new `Key::Named(NamedKey::Space) => { self.workspace_notes_modal.push_char(' '); self.sync_workspace_notes_draft(); self.request_redraw(); }` arm in `handle_workspace_notes_keyboard` in `crates/scribe-client/src/main.rs` (same match block as T002). If quickstart V5 reveals that winit delivers spacebar as `Key::Character(" ")` on this platform, leave the existing `Key::Character` arm path and verify it covers `' '` (research.md open risk #1); document the actual event shape in the task completion notes.
- [X] T004 [P] Add the `AddingNoteState` struct in `crates/scribe-client/src/workspace_notes.rs` per `data-model.md` — fields `draft_text: String`, `draft_dirty: bool`, `caret_byte: usize`, `scroll_offset_rows: usize`, `last_server_error: Option<String>`, `committed_pending: bool`. Plus helpers `new_from_saved_draft`, `insert_char`, `backspace`, `move_caret_*`, `is_blank_trimmed`.
- [X] T005 Add per-workspace state map field `adding_note_states: HashMap<WorkspaceId, AddingNoteState>` on the `App` struct in `crates/scribe-client/src/main.rs` (next to `workspace_notes_save_pending`) and initialize it as `HashMap::new()` in the constructor. (Note: HashMap not BTreeMap — `WorkspaceId` doesn't implement Ord.)
- [X] T006 [P] Extended `WorkspaceNotesPreviewInteraction` in `crates/scribe-client/src/workspace_notes_preview.rs` with `affordance_rect: Option<Rect>` field. Populated as `None` for now; T011 will compute the real value.
- [X] T007 [P] Sanity-checked column-width math in `PreviewLayout::new`: `MIN_PREVIEW_COLS = 22` already accommodates the affordance's ~2-col width plus padding. No math change needed.
- [X] T008 Extended `flush_workspace_notes_now` and added new `flush_inline_editor_drafts` in `crates/scribe-client/src/main.rs` (around line 9104) — modal flush + per-workspace inline editor flush + timer reset.
- [X] T009 [P] Added `App::dirty_inline_editor_drafts(&self) -> Vec<(WorkspaceId, String)>` helper.
- [X] T010 [P] Added `App::workspace_has_inline_editor(&self, ws: WorkspaceId) -> bool` predicate.

**Checkpoint**: Phase 2 leaves the modal editor with the new keymap working and the spacebar bug fixed; the inline editor's data plumbing is in place but the UI surface for it is not yet wired. The codebase still builds cleanly. T002 and T003 alone are independently shippable as a separate commit if desired.

---

## Phase 3: User Story 1 — Inline capture from hover preview (Priority: P1) 🎯 MVP

**Goal**: A user can hover the workspace badge, click "+", type a multi-line note, and commit it via Enter without ever opening the notes modal.

**Independent Test** (from spec): Hover the workspace badge, click the "+" affordance, type a multi-line note (use Ctrl+Enter for a newline), commit it with Enter, and confirm that the new entry appears in the hover preview's active list and is persisted by the server (visible from a second client window).

### Implementation for User Story 1

- [X] T011 [US1] Render the "+" affordance cell inside `build_workspace_notes_preview` in `crates/scribe-client/src/workspace_notes_preview.rs` — emit `CellInstance` quads for a `~2 cols × 1 row` bordered cell in the bottom-right of the preview, using the existing chrome border treatment from `ChromeColors`. Draw distinct idle / hover / pressed / disabled visual states per FR-001 + UX-002. The pressed state is keyed off the current mouse-button-down event when the pointer is over `affordance_rect` from T006.
- [X] T012 [US1] Wire affordance hit-routing in `apply_workspace_notes_preview_overlay` in `crates/scribe-client/src/main.rs` (around line 4806) — if the preview's `WorkspaceNotesPreviewInteraction.affordance_rect` contains the click point, call a new `App::open_inline_note_editor(workspace_id)` helper that: looks up the workspace's saved draft via `WorkspaceNotesStore`, constructs `AddingNoteState::new_from_saved_draft(...)` (per FR-002), inserts it into `adding_note_states` keyed by workspace, requests a redraw, and returns. Depends on T004, T005, T006, T010, T011.
- [X] T013 [US1] Render the inline editor row inside `build_workspace_notes_preview` in `crates/scribe-client/src/workspace_notes_preview.rs` when the build context is told the workspace has an `AddingNoteState`. The editor row sits below the note list, takes the affordance's previous slot (the "+" is hidden while editing per FR-002), uses caret + padding + background contrast distinct from read-only rows per UX-004. Extend `WorkspaceNotesPreviewBuildContext` with an `inline_editor: Option<&AddingNoteState>` field; the caller in `main.rs` passes the value from `adding_note_states.get(&workspace_id)`. Depends on T004, T011.
- [X] T014 [US1] Route keyboard input to the inline editor when its workspace's preview is rendered. In the focused-pane key dispatch in `crates/scribe-client/src/main.rs`, before PTY translation, check `App::workspace_has_inline_editor(&self, hovered_workspace)` (T010). If true, delegate to a new shared `App::handle_workspace_notes_inline_keyboard(&mut self, ws: WorkspaceId, event: &KeyEvent)` that uses the same keymap as `handle_workspace_notes_keyboard` (Enter → save, Ctrl+Enter → newline insert, Escape → cancel, Backspace → pop_char, `NamedKey::Space` → push `' '`, `Key::Character(text)` → push non-control chars). Depends on T002, T003, T005, T010.
- [X] T015 [US1] On any keystroke that mutates the inline editor's `draft_text` (T014's text-mutating arms), set `AddingNoteState::draft_dirty = true`, schedule a `SaveDraft` flush via the existing `WORKSPACE_NOTES_DEBOUNCE` window using the same path the modal uses (so PR-002's bandwidth bound is preserved). Reuse the existing `workspace_notes_save_pending: Option<Instant>` debounce timer; `flush_workspace_notes_if_due` is already extended by T008 to emit per-workspace `SaveDraft`. Depends on T008, T009, T014.
- [X] T016 [US1] In `App::handle_workspace_notes_inline_keyboard` (T014), implement the Enter-commit path per FR-006/016: if `draft_text.trim().is_empty()`, delegate to T026's no-op handling; otherwise set `committed_pending = true`, emit `WorkspaceNotesMutation::CreateActiveNote { workspace_id, text: draft_text.clone() }` through `cmd_tx`, and short-circuit any subsequent Enter presses while `committed_pending == true` (coalescing per FR-016). Depends on T013, T014.
- [X] T017 [US1] In the existing `WorkspaceNotesChanged` server-broadcast handler in `crates/scribe-client/src/main.rs`, when the broadcast resolves a workspace that has `committed_pending == true` in `adding_note_states` AND the broadcast's active-note list grew, remove the workspace's `AddingNoteState` entry, request a redraw of that workspace's preview so the new entry appears in the read-only list, and ensure the `SaveDraft` buffer is empty afterward per FR-006 + FR-020. Depends on T016.

**Checkpoint**: At this point a user can hover → click "+" → type → press Enter → see the new active note. Pointer-leave behavior, cross-workspace state, and cancel/error paths are still default (read-only-preview behavior); these are added in US2 and US3.

---

## Phase 4: User Story 2 — Hover-dismiss suppression and per-workspace state (Priority: P2)

**Goal**: An inline-editor draft survives pointer movement off the workspace badge and the preview bounds, and multiple workspaces can hold concurrent editor states without interfering with each other.

**Independent Test** (from spec): Click "+", type a few characters, deliberately move the pointer off both the workspace badge and the preview bounds, wait several seconds, and confirm the preview remains open with the typed text intact and the caret still active. Additionally (V2b/V2c from quickstart.md): pointer-move from workspace A to workspace B preserves A's editor state and lets B independently open its own editor.

### Implementation for User Story 2

- [X] T018 [US2] In the hover-preview lifecycle logic in `crates/scribe-client/src/main.rs` (the pointer-leave path that today calls `apply_workspace_notes_preview_overlay` with an empty preview), suppress the auto-close when `App::workspace_has_inline_editor(hovered_workspace)` is true (FR-003). Pointer-leave to empty terminal space MUST leave the preview visible. Depends on T010, T012.
- [X] T019 [US2] Implement pointer-return restoration in the preview render path — when the pointer enters a workspace badge whose `adding_note_states` entry is non-empty, render the preview with the inline editor row already open (caret, draft text, scroll offset, last server error all restored from the persisted `AddingNoteState`) per FR-003 + FR-021. Depends on T013, T018.
- [X] T020 [US2] Wire higher-priority-overlay handoff per FR-010 — in each overlay-open code path in `crates/scribe-client/src/main.rs` (`open_workspace_notes_modal` at line 9556, the context-menu open path, command-palette open, search-overlay open, close-dialog open, update-dialog open), call `App::flush_workspace_notes_now` (extended in T008) and then `adding_note_states.clear()`. After clearing, request a redraw so any visible previews drop their editor rows. Depends on T008, T009.
- [X] T021 [US2] Remove the implicit "focus-change closes inline editor" coupling that the prior singleton design would have introduced. Audit `crates/scribe-client/src/main.rs` for any focus-change handler that touches `workspace_notes_save_pending` or invokes `flush_workspace_notes_now`, and confirm it does NOT clear `adding_note_states`. Per FR-013, a bare focus change without an overlay open MUST NOT exit any inline editor. Depends on T005.
- [X] T022 [US2] Extend the existing window-close / app-shutdown / update-relaunch deferral path in `crates/scribe-client/src/main.rs` (search for `flush_workspace_notes_now` callers that gate quit) so it also defers until `adding_note_states` is empty after the flush completes, per FR-014. Depends on T008.
- [X] T023 [US2] Ensure the read-only preview render path correctly handles split-pane layouts where multiple workspace badges are simultaneously visible — each badge's preview MUST be able to render its own editor independently (per FR-021). Audit the existing preview-render call site to confirm it is invoked per-workspace, not as a singleton. Depends on T013, T019.
- [X] T024 [US2] Confirm that the snapshot-pristine policy in `crates/scribe-client/src/workspace_notes.rs` extends to inline drafts — when a `WorkspaceNotesChanged` snapshot arrives that contains a saved draft AND `adding_note_states.get(&workspace).is_some()` AND that entry's `draft_dirty == true`, the snapshot MUST NOT overwrite `AddingNoteState::draft_text` (FR-015). If `draft_dirty == false`, the snapshot may re-hydrate the draft. Depends on T004, T005, T017.

**Checkpoint**: At this point inline editors survive pointer movement, focus changes, and split-pane workflows. Multiple workspace previews can hold independent editor state. Cancel and abandon-empty paths still default to the read-only fallback in trivial cases but lack the explicit Escape semantics — those land in US3.

---

## Phase 5: User Story 3 — Cancel, abandon-empty, and server-error recovery (Priority: P3)

**Goal**: Users can back out of an unintended "+" click, abandon a whitespace-only draft, and recover from a server rejection without losing their typed text.

**Independent Test** (from spec): Click "+" without typing, press Escape, confirm the preview returns to read-only with no entry created and no workspace draft written. Repeat with whitespace-only text + Enter, and again with real text + Escape. Plus quickstart V3c — simulate a server `CreateActiveNote` rejection and verify the editor row preserves text and surfaces a retryable error.

### Implementation for User Story 3

- [X] T025 [US3] In `App::handle_workspace_notes_inline_keyboard` (T014), implement the Escape branch per FR-008 — remove the workspace's entry from `adding_note_states`, do NOT emit `SaveDraft`, request a redraw so the preview returns to its read-only state with the "+" affordance restored. The discard MUST NOT touch the workspace's existing saved draft on the server. Depends on T014.
- [X] T026 [US3] In the Enter-commit path (T016), implement the empty/whitespace-only short-circuit per FR-007 — if `draft_text.trim().is_empty()`, remove the workspace's `AddingNoteState` entry without emitting `CreateActiveNote`, do NOT flush `SaveDraft`, request a redraw. Depends on T016.
- [X] T027 [US3] Extend the `WorkspaceNotesChanged` / `Error` server-message handler in `crates/scribe-client/src/main.rs` to detect rejection of a `CreateActiveNote` that originated from a workspace currently holding an `AddingNoteState` — set `last_server_error` to the rejection text, set `committed_pending = false`, preserve `draft_text`, request a redraw so the error is visible per FR-012. Depends on T016, T017.
- [X] T028 [US3] Render the `AddingNoteState.last_server_error` text in the inline editor row's chrome in `crates/scribe-client/src/workspace_notes_preview.rs`, matching the modal's existing retryable-server-error visual treatment (the same `ChromeColors` palette + same prefix/spacing as the modal's footer error) per UX-001. Depends on T013, T027.

**Checkpoint**: All three user stories are functional. The remaining tasks address growth/scroll, the click-archive coexistence with the editor, lat.md sync, and manual verification.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Implement the growth/scroll model (FR-019, FR-022), confirm existing archive clicks still work alongside the editor (FR-011), update lat.md, and run all manual verifications.

- [X] T029 [P] Implement the 3/4-pane growth cap per FR-019 in `PreviewLayout::new` (`crates/scribe-client/src/workspace_notes_preview.rs` around line 79). Compute the cap as `3 * focused_pane_height / 4` (using the existing `viewport` rect's height OR a new `pane_height` parameter passed by `apply_workspace_notes_preview_overlay`); set the preview's outer height to `min(content_height, cap)` when an `AddingNoteState` is active. Re-evaluate on every render so pane resizes immediately re-apply.
- [X] T030 [P] Implement caret-tracking auto-scroll per FR-022 (first input) in `crates/scribe-client/src/workspace_notes_preview.rs` — when the editor's caret would render outside the visible rows, adjust `AddingNoteState::scroll_offset_rows` so the caret line stays visible. Triggered by any keystroke that moves the caret.
- [X] T031 [P] Implement mouse-wheel scroll inside the editor row per FR-022 (second input) — in the mouse-wheel handler in `crates/scribe-client/src/main.rs`, when the wheel event lands inside the editor row's rect AND that workspace has an `AddingNoteState`, update `scroll_offset_rows` without moving the caret. Wheel events OUTSIDE the editor row's rect (still over the preview) MUST NOT consume the wheel; let them fall through to the existing read-only preview wheel behavior (typically none). Depends on T013.
- [X] T032 [P] Integrate an overlay scrollbar inside the editor row per FR-022 (third input) — reuse the existing `ScrollbarState` from `crates/scribe-client/src/scrollbar.rs` (1.5 s idle fade, 0.3 s fade-out, hover-expand width, 3× hit zone, drag-to-scroll computing offset from mouse delta). Drive it from `AddingNoteState::scroll_offset_rows` and the editor's content height. The scrollbar MUST be local to the editor row and MUST NOT render alongside the terminal-scrollback scrollbar.
- [X] T033 [P] Audit click routing in `apply_workspace_notes_preview_overlay` (`crates/scribe-client/src/main.rs` around line 4806) so that clicks on existing read-only note rows still emit `ArchiveNote { reason: Done }` while the editor is open (FR-011), AND clicks on the editor row itself do NOT trigger archival. Concretely: hit-test the editor row's rect BEFORE the read-only note rows' rects; clicks landing in the editor row are absorbed (focus the editor / set caret position by mouse if the implementation supports it) without producing a note-archival action.
- [X] T034 Update `lat.md/client.md` Workspace Notes section to describe: the "+" affordance (~2 col × 1 row bordered cell, bottom-right, `ChromeColors` states), the per-workspace `AddingNoteState` lifecycle, the 3/4-pane growth cap + 3-input scroll model, the modal keymap flip (Enter saves, Ctrl+Enter inserts a newline), and the spacebar fix. Update any code refs (`[[crates/.../...]]`) to point at the new helpers introduced by T004 / T010 / T012 / T014. Do NOT change `lat.md/protocol.md` or `lat.md/server.md` (per contracts/README.md).
- [X] T035 Run `lat check` from the repo root; all wiki links and code refs MUST pass before this task is considered complete.
- [ ] T036 Run the manual quickstart suite V1–V6 from `specs/007-add-note-from-hover/quickstart.md` against a freshly-launched client; record pass/fail for each recipe including the performance probes (PR-001, PR-002, PR-003). Note any deviation from spec behavior.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: No dependencies — can start immediately.
- **Phase 2 (Foundational)**: Depends on Phase 1. **BLOCKS all user stories.** Within Phase 2, T002 and T003 are independent and can run in parallel with each other; T004 → T005 sequentially; T005 → T008/T009/T010; T006 → T007. T011 (US1) cannot start until T006 lands.
- **Phase 3 (US1)**: Depends on Phase 2. Implementation order within US1: T011 → T012/T013 (parallel-ish; T012 needs T011's affordance render to hit-test, T013 needs T011 to know where to place the editor row) → T014 → T015 / T016 → T017. None of these are [P] within US1 because they touch overlapping state and chain logically.
- **Phase 4 (US2)**: Depends on Phase 3 (the editor must exist before its dismiss behavior can be tested). T018–T024 within US2 are mostly sequential due to overlapping touch points in `main.rs`.
- **Phase 5 (US3)**: Depends on Phase 3 (T014, T016) but is independent of Phase 4. T025–T028 within US3 are sequential.
- **Phase 6 (Polish)**: T029, T030, T031, T032, T033 are independent of each other (different concerns), depend on T013 (editor render exists), and can be parallelized. T034 → T035 → T036 must run sequentially at the end.

### User Story Dependencies

- **US1 (P1)**: Can start after Phase 2. No dependencies on other stories. Is the MVP.
- **US2 (P2)**: Conceptually independent (per spec) but in practice requires US1's `AddingNoteState` lifecycle to be in place. Can be cleanly tested independently against the V2 quickstart recipes once US1 lands.
- **US3 (P3)**: Conceptually independent but requires US1's Enter-commit path (T016) and key dispatcher (T014). Tested independently against V3 quickstart recipes.
- US2 and US3 do NOT depend on each other — once US1 ships, US2 and US3 can be implemented in parallel by different team members if desired.

### Parallel Opportunities

- T002 and T003 (Phase 2) — independent match arm changes; could be one commit or two.
- T004 and T006 (Phase 2) — different files entirely.
- T008, T009, T010 (Phase 2) — different helpers; all depend on T005 but not on each other.
- US2 and US3 can be parallelized after US1 lands.
- All of T029, T030, T031, T032, T033 in Phase 6 can be parallelized (different concerns in possibly overlapping files but non-overlapping regions).

---

## Parallel Example: Phase 2 Foundational

After T001 (build baseline) and T005 (App field) land sequentially:

```bash
# Parallel batch A — keymap & spacebar (independent match arms in same function):
Task: "T002 Flip the modal-editor commit keymap (Enter↔Ctrl+Enter) in handle_workspace_notes_keyboard"
Task: "T003 Add NamedKey::Space arm in handle_workspace_notes_keyboard"

# Parallel batch B — data plumbing (different files / sections):
Task: "T004 Add AddingNoteState struct in crates/scribe-client/src/workspace_notes.rs"
Task: "T006 Extend WorkspaceNotesPreviewInteraction with affordance_rect in workspace_notes_preview.rs"

# Parallel batch C — helpers (after T005):
Task: "T008 Extend flush_workspace_notes_now to iterate adding_note_states"
Task: "T009 Add dirty_inline_editor_drafts helper"
Task: "T010 Add workspace_has_inline_editor predicate"
```

## Parallel Example: Phase 6 Polish

After US1+US2+US3 land:

```bash
Task: "T029 Implement 3/4-pane growth cap in PreviewLayout::new"
Task: "T030 Implement caret-tracking auto-scroll for inline editor"
Task: "T031 Implement mouse-wheel scroll inside editor row"
Task: "T032 Integrate ScrollbarState overlay scrollbar inside editor"
Task: "T033 Audit click routing to preserve archive clicks alongside editor"
```

---

## Implementation Strategy

### MVP First (US1 only)

1. Complete Phase 1 (T001) + Phase 2 (T002–T010) — the modal-side keymap flip + spacebar fix already ship as a discrete user-visible improvement at this point.
2. Complete Phase 3 (T011–T017) — inline capture works end-to-end.
3. **STOP and VALIDATE** with quickstart V1 + V4 + V5 (US1 path + modal keymap probe + spacebar fix probe).
4. Ship.

### Incremental Delivery

1. Complete Phases 1 + 2 → modal keymap flip and spacebar fix are live. Validate with V4 + V5. Ship.
2. Add Phase 3 → US1 inline capture is live. Validate with V1. Ship.
3. Add Phase 4 → US2 dismiss-suppression and per-workspace isolation. Validate with V2a/b/c/d. Ship.
4. Add Phase 5 → US3 cancel + error recovery. Validate with V3a/b/c. Ship.
5. Add Phase 6 → growth, scroll, click-coexistence, lat.md, manual quickstart V6 + performance probes. Final ship.

### Parallel Team Strategy

After Phase 2 lands:

- Developer A: US1 (T011–T017) — must complete first; gate for B and C.
- Developer B (starts when US1's T013 + T014 land): US2 (T018–T024).
- Developer C (starts when US1's T016 lands): US3 (T025–T028).
- All three converge on Phase 6 (T029–T036).

---

## Notes

- **No new automated tests requested.** Manual verification only, per spec QR-002 and constitution principle II. Skip any TDD framing; do not add test-writing tasks.
- **No server restart involved.** All work is client-only changes plus a `lat.md/client.md` doc update.
- **Modal keymap flip is a breaking UX change** for existing users (Enter↔Ctrl+Enter); no migration toggle is provided. Documented in spec Assumption #8.
- **Spacebar fix is a real bug fix** the user surfaced during clarification (FR-018). It ships in Phase 2 alongside the keymap flip because both live in the same `handle_workspace_notes_keyboard` function.
- **Commit cadence**: T002 + T003 (Phase 2 keymap-and-bug-fix) is a natural early commit. T004–T010 (Phase 2 data plumbing) is a second natural commit. US1 land as one commit. US2 and US3 each land as commits. Polish phase splits across one or two commits.
- **Constitution gates** are re-verified by T034 (`lat.md` sync) + T035 (`lat check`) + T036 (quickstart). The plan's post-design Constitution Check already documented PASS for all five gates; T036 records the verification commands actually run, per the constitution's Development Workflow requirement.
- **Avoid**: vague tasks (each task has a file path + region), same-file conflicts within a phase (Phase 2's T002/T003 are in the same function but non-overlapping match arms; flagged in their descriptions), cross-story dependencies that break independent verification (US2 and US3 are independent of each other; both depend on US1 which is the MVP).
