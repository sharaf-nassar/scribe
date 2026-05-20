# Quickstart: Add Note From Hover Preview

**Date**: 2026-05-20
**Spec**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md)

This document defines the **manual verification recipes** for each user story plus targeted probes for the modal keymap flip (FR-017) and the spacebar fix (FR-018). Per spec QR-002 and constitution principle II, this feature has no new automated tests; these recipes are the verification path.

## Setup

1. Build the client and server:
   ```
   cargo build -p scribe-client -p scribe-server
   ```
2. Start a Scribe session with at least two workspaces visible in the tab bar. (Open two workspace badges using the existing workspace-add flow.) For the per-workspace isolation probe in **V2**, you'll want them side-by-side.
3. Make sure no notes modal is currently open. The hover preview is the entry point for every recipe below.

> **Server restart policy**: Do NOT restart the server (`just restart-server` / `--upgrade`) during these verifications without explicit approval. The client changes are hot-reloadable; the server is unchanged.

---

## V1 — User Story 1: Inline capture from hover preview (P1)

**Verifies**: FR-001, FR-002, FR-004, FR-005, FR-006, FR-009, FR-019, FR-020.

1. Hover the workspace badge for workspace **A**. Confirm the preview opens with its read-only list.
2. Confirm a **bordered "+" cell** is visible in the **bottom-right corner** of the preview (~2 columns × 1 row), distinct from the note rows.
3. Move the pointer over the "+" cell. Confirm the cell's **hover state** activates (background/border change per `ChromeColors`).
4. Click the "+" cell. Confirm:
   - The preview transitions into "adding note" state with a new editor row at the bottom of the list.
   - The editor's caret is **immediately ready** (no extra click required).
   - The "+" affordance is hidden or disabled.
   - If workspace A had a saved draft (set in the modal earlier), the editor row is **pre-populated** with that text and the caret is at the end.
5. Type a few characters. Confirm they appear in the editor row.
6. Press **Ctrl+Enter**. Confirm a literal newline is inserted at the caret and the editor row grows to a second visual line.
7. Type more text on the second line.
8. Press **Enter**. Confirm:
   - The mutation `CreateActiveNote` is sent (visible by the new entry appearing in the preview list within one render tick after the server broadcasts).
   - The preview returns to read-only state with the "+" affordance restored.
   - Opening the modal for workspace A now shows an **empty** New-note editor (saved draft was consumed; FR-006 / FR-020).

**Pass criteria**: All six observations hold without a modal opening at any step.

---

## V2 — User Story 2: Hover-dismiss suppression and per-workspace state (P2)

**Verifies**: FR-003, FR-010, FR-013, FR-021, plus the cross-workspace and pointer-return edge cases.

### V2a Pointer-leave does not close the editor

1. Hover workspace **A**'s badge → preview opens.
2. Click "+". Type "TODO X".
3. Move the pointer far away from both the badge and the preview, into empty terminal space.
4. Wait 5 seconds.
5. Move the pointer back to workspace **A**'s badge.

**Pass criteria**: The preview re-opens with the editor still in "adding note" state, "TODO X" intact, caret at end.

### V2b Cross-workspace pointer movement preserves state

1. (Continuing from V2a or freshly) hover workspace **A**, click "+", type "FOR A".
2. Move the pointer to workspace **B**'s badge.

**Pass criteria**: B's preview opens in read-only state (or with its own editor state if previously opened). A's editor is preserved.

3. Click "+" on B's preview. Type "FOR B".

**Pass criteria**: Both A and B have independent open editors.

4. Move pointer back to A's badge.

**Pass criteria**: A's preview opens with "FOR A" intact and editor active.

### V2c Focus change does not close editors

1. With A's editor open (from V2a/b), click inside workspace **B**'s terminal pane to change focus.

**Pass criteria**: A's preview hides (pointer left), but on returning to A's badge, A's editor restores intact (no `SaveDraft` flush yet because the focus change alone shouldn't trigger one — the editor's state is preserved by the workspace, not by focus).

### V2d Higher-priority overlay flushes the draft

1. With A's editor open and "FOR A" typed, open the command palette (use the configured shortcut).
2. Cancel the command palette.
3. Open the modal for workspace **A** by clicking its badge.

**Pass criteria**:
- A's editor was exited when the command palette opened (FR-010).
- "FOR A" was flushed via `SaveDraft` and now appears as the **pre-filled draft** in A's modal New-note editor when it opens (FR-020).

---

## V3 — User Story 3: Cancel and abandon-empty semantics (P3)

**Verifies**: FR-007, FR-008, FR-012, plus the `Escape` and whitespace-only paths.

### V3a Escape discards typed text

1. Hover workspace A. Click "+". Type "DRAFT TO CANCEL".
2. Press **Escape**.

**Pass criteria**:
- Preview returns to read-only state.
- No new active note is created.
- Opening A's modal afterwards shows an **empty** draft (Escape did not flush via `SaveDraft` per FR-008).

### V3b Whitespace-only Enter is a no-op

1. Click "+". Press space a few times. Press **Enter**.

**Pass criteria**:
- No `CreateActiveNote` is sent.
- No active note is created.
- Preview returns to read-only state.
- No `SaveDraft` write occurs (or if one does, the saved draft is empty after).

### V3c Server-error retain-and-retry

1. Click "+". Type "should fail".
2. **Simulate a server error**: easiest reproducer is to disconnect the IPC socket briefly OR have the server return an error for the `CreateActiveNote` (instrument-friendly point). If a clean reproducer is not available, document the simulation method used.
3. Press **Enter**.

**Pass criteria**:
- The editor row preserves "should fail" verbatim.
- A retryable error message is rendered in the same chrome the modal uses for note errors.
- The preview remains in "adding note" state until the user retries (Enter again) or cancels (Escape).
- After clearing the failure condition and pressing Enter again, the note creates successfully.

---

## V4 — Modal keymap flip probe (FR-017)

**Verifies**: the existing modal editor now matches the inline editor's keymap.

1. Click a workspace badge (not "+") to open its **modal**.
2. The New-note editor area is focused.
3. Type "hello".
4. Press **Ctrl+Enter**.

**Pass criteria**: A literal newline is inserted at the caret (not a save). Type "world" on the new line so the modal now shows "hello\nworld".

5. Press **Enter**.

**Pass criteria**: The modal saves the note. A new active entry "hello\nworld" appears in the modal's note list.

6. Press **Escape**.

**Pass criteria**: The modal closes (Escape behavior is unchanged from the prior modal binding).

---

## V5 — Modal spacebar bug fix probe (FR-018)

**Verifies**: the existing modal editor accepts spacebar input.

1. Open the modal for a workspace.
2. Type "the quick brown fox".

**Pass criteria**: All spaces appear in the draft. The string in the editor is exactly "the quick brown fox" with the four spaces present.

3. Press **Enter** to save.

**Pass criteria**: The saved active note's text is "the quick brown fox" (verified by the note appearing in the modal's active list and matching what was typed).

If the typed text shows up as "thequickbrownfox" with no spaces, the spacebar fix is not yet in place.

---

## V6 — Overflow and scroll (FR-019, FR-022)

**Verifies**: the 3/4-pane height cap and the three scroll inputs.

1. Resize the Scribe window so the focused workspace's pane is **tall** (a normal terminal height).
2. Hover A's badge, click "+", and type enough text to overflow ~3/4 of the pane height (paste a long block of lorem ipsum if convenient).
3. Confirm:
   - The preview grows as text is added.
   - Growth caps at ~3/4 of the focused pane's vertical extent.
   - Once the cap is hit, the editor row scrolls internally to keep the caret visible (caret-tracking auto-scroll, FR-022 first input).
4. Click into the editor row's body (not the affordance area) and use the **mouse wheel** to scroll the editor up to see earlier text.
   - **Pass criteria**: The viewport scrolls without moving the caret.
   - Scroll-wheel events ABOVE the editor row (e.g., over the read-only rows or the preview border) should NOT scroll the editor (FR-022 mouse-wheel scope).
5. Hover the **overlay scrollbar** that fades in on the editor's right edge during scrolling.
   - **Pass criteria**: The scrollbar follows the `ScrollbarState` pattern — fades in on scroll, expands width on hover, drag-to-scroll works.
6. Resume typing.
   - **Pass criteria**: The viewport snaps back to keep the caret visible.

---

## V7 — UTF-8 / multi-byte caret math probe (FR-005, FR-009, regression for caret-up/down)

**Verifies**: arrow-key navigation, Backspace, and Ctrl+Enter newline insert behave correctly when the draft contains multi-byte characters. Catches the byte-as-column bug that earlier versions of `move_caret_up` / `move_caret_down` exhibited.

1. Hover workspace A's badge → click "+".
2. Type a line containing CJK + emoji + Latin: e.g. `第一行 hello 😀`.
3. Press **Ctrl+Enter** to insert a newline.
4. Type a second line: `second`.
5. Press **ArrowUp**.
   - **Pass criteria**: the caret lands somewhere visually in the middle of the first line (not after the 6-character emoji boundary mid-codepoint, not past the end of line). The next typed character should appear at the visual column the caret indicates.
6. Press **End** / **Home** on the first line, then **ArrowDown**.
   - **Pass criteria**: caret moves to the corresponding visual column on the second line; if the second line is shorter, the caret clamps to the end of the second line (not into the middle of a multi-byte char).
7. Press **Backspace** repeatedly to delete the emoji.
   - **Pass criteria**: emoji disappears in a single Backspace (it's a single multi-byte char); cursor stays at a valid char boundary.

If the caret ever lands inside a multi-byte sequence the editor will panic with a UTF-8 boundary error from `String::insert` / `String::replace_range` — that's the failure mode this test catches.

---

## V8 — FR-014 quit-time flush probe

**Verifies**: window close / app shutdown defers until in-flight inline-editor drafts have been flushed through `SaveDraft`.

1. Hover workspace A → click "+". Type "QUIT TEST 1".
2. Trigger window close (Cmd/Ctrl+W or the platform close shortcut).

**Pass criteria**:
- The window does not close instantly; the app defers the close until the `SaveDraft` ack arrives (this may be visible only as a single-frame latency, but the test point is *no data loss*).
- On a fresh app launch, opening the modal for workspace A shows "QUIT TEST 1" as the pre-filled draft in the New-note editor.

---

## V9 — FR-015 pristine-draft policy probe

**Verifies**: a late `WorkspaceNotesChanged` snapshot does NOT overwrite a dirty inline-editor draft.

1. Workspace A has a saved draft "old draft" (set via the modal). Close the modal.
2. Hover workspace A → click "+". The editor pre-populates with "old draft".
3. Append " edits" so the editor reads "old draft edits"; `draft_dirty = true`.
4. From a **second connected client window**, open the modal for workspace A, type a different value "remote update", and save it (commits a SaveDraft on the server side; first client receives `WorkspaceNotesChanged`).
5. Return to the first client.

**Pass criteria**: the first client's inline editor still shows "old draft edits" — the late snapshot did not clobber the dirty local draft. (After commit/cancel, the next open of the editor will see "remote update" since the server now considers that the canonical draft.)

---

## V10 — FR-016 commit coalescing probe

**Verifies**: rapid successive Enter presses do not send duplicate `CreateActiveNote` mutations.

1. Hover workspace A → click "+". Type "COALESCE".
2. Press **Enter** five times rapidly.

**Pass criteria**:
- Exactly ONE new active note "COALESCE" appears in workspace A's active list.
- No duplicate notes appear from the queued Enters.
- After the server broadcast resolves, the preview returns to read-only state with the "+" affordance restored.

Instrumentation-friendly variant: tail IPC logs for `CreateActiveNote { workspace_id, text: "COALESCE" }` — exactly one should be sent.

---

## V11 — FR-011 archive-while-editing probe

**Verifies**: clicks on existing read-only note rows in the preview continue to archive them while the inline editor is open; the editor row's hit-rect doesn't shadow the note-row hit-rects.

1. Ensure workspace A has at least two active notes ("alpha", "beta").
2. Hover workspace A → click "+". Type "still typing".
3. Click on the read-only row for "alpha".

**Pass criteria**:
- "alpha" is archived as Done (disappears from the active list).
- The inline editor stays open with "still typing" intact.
- The "+" affordance remains hidden / disabled (editor is still in adding-note state).
- The preview re-renders to reflect the now one-shorter active list.

4. Click on the editor row itself (not on any note row above it).

**Pass criteria**: nothing is archived; the editor row absorbs the click; caret may or may not move depending on implementation, but no note row is affected.

---

## Performance budget probes (PR-001, PR-002, PR-003)

| Budget | Probe |
|---|---|
| PR-001 — "+" click → caret-ready in 1 render tick | Visually observe; for instrumentation, the existing render-tick log should show no extra ticks between the click event and the first editor-row paint. |
| PR-002 — `SaveDraft` debounce bounds keystroke bandwidth | Type a long string quickly inside the inline editor; confirm `SaveDraft` writes are sent at the existing debounce cadence (not per-keystroke). |
| PR-003 — Zero added per-frame cost for non-engagers | Hover the workspace badge without clicking "+"; the read-only preview should render identically to the `004-workspace-notes` baseline (no new allocations per frame; no new state lookups). |

## Post-task checklist

Before reporting completion of `/speckit-implement`:

1. Run `cargo build -p scribe-client -p scribe-server` — clean build.
2. Run the full quickstart V1–V6 above on a freshly-launched client.
3. Run V4 and V5 specifically on a workspace that had a saved draft prior to the upgrade (regression sanity for the breaking modal keymap change).
4. Update `lat.md/client.md` Workspace Notes section to reflect the new behavior (affordance, per-workspace editor state, scroll model, keymap flip, spacebar fix).
5. Run `lat check` — all wiki links and code refs MUST pass.
6. Confirm no `lat.md/server.md` or `lat.md/protocol.md` changes were needed (per contracts/README.md).
