# Feature Specification: Add Note From Hover Preview

**Feature Branch**: `007-add-note-from-hover`
**Created**: 2026-05-19
**Status**: Draft
**Input**: User description: "we want to add a way to add notes directly from the hover menu. there should be a + button in the bottom right corner cleanly styled and spaced and once clicked it should add a new empty task to the list and allow us to type it in where the input/task grows as there is more text and everything scales correctly. once the add button is clicked the hover disappear should get disabled to avoid closing it while the user is editing"

## Context

This feature extends the existing workspace-notes hover preview (introduced in feature `004-workspace-notes`) with an inline note-creation affordance. The hover preview is the lightweight, read-only overlay that already appears when the user mouses over a workspace badge — it lists active notes and supports single-click archival of an existing note. Today, capturing a *new* note still requires opening the notes modal (clicking the workspace badge), which interrupts the user's flow.

The goal is to let a user start, type, and commit a new active note entirely inside the hover preview, while preserving every existing hover-preview behavior (read-only glance, click-to-archive on existing rows, draft-preserving close on overlay handoff) for users who never engage the new affordance.

In service of "one editor mental model" across modal and hover-preview entry points, this feature also (1) **aligns the existing modal editor's keymap with the new inline editor's**: Enter commits the active draft and Ctrl+Enter inserts a newline (this is the inverse of the modal's prior binding and matches the in-terminal Ctrl+Enter newline convention used elsewhere in Scribe); and (2) **fixes a pre-existing modal bug** where the spacebar fails to insert a space character into the modal's draft.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Inline Capture From Hover Preview (Priority: P1)

A Scribe user hovers the workspace badge, clicks the "+" affordance in the bottom-right of the preview, types a multi-line note inline, and commits it — without ever opening the notes modal.

**Why this priority**: This is the entire point of the feature. The value of inline capture vanishes if the user still has to open the modal to commit; everything else (cancel paths, hover-dismiss suppression, growing input) only matters once this core path works.

**Independent Test**: Hover the workspace badge, click the "+" affordance, type a multi-line note (including a newline), commit it, and confirm that the new entry appears in the hover preview's active list and is persisted by the server (visible from a second client window).

**Acceptance Scenarios**:

1. **Given** the hover preview is visible over a workspace badge, **When** the user looks at the preview, **Then** a discoverable "+" affordance is present in the preview's bottom-right corner with enough spacing from the surrounding rows to read as a distinct control.
2. **Given** the "+" affordance is visible, **When** the user clicks it, **Then** the preview transitions to an "adding note" state with an inline editor row appended below the existing list, a visible caret ready to receive input, and the "+" affordance hidden or disabled so a second concurrent draft cannot be started.
3. **Given** the inline editor is focused, **When** the user types characters and presses Ctrl+Enter, **Then** characters appear in the editor row, a literal newline is inserted at the caret, and the row grows to a second line so the entire draft remains visible.
4. **Given** the user has typed non-empty text in the inline editor, **When** the user presses Enter, **Then** the draft is sent through the existing `CreateActiveNote` mutation, the server broadcasts the new collection, and the preview returns to read-only state showing the new entry in its active list.
5. **Given** a second client window is connected to the same workspace, **When** the first client commits an inline new note, **Then** the second client's hover preview reflects the new entry on its next render without any user action.

---

### User Story 2 - Hover-Dismiss Suppression While Editing (Priority: P2)

A Scribe user who has clicked "+" and started typing can move the pointer freely (e.g., to copy text from the terminal behind, to reach for the keyboard) without the preview closing and losing their in-progress note.

**Why this priority**: Without this, inline capture is unusable in practice — every pointer twitch outside the badge would close the overlay and discard typed text. This is the explicit blocking requirement from the request ("the hover disappear should get disabled to avoid closing it while the user is editing").

**Independent Test**: Click "+", type a few characters, deliberately move the pointer off both the workspace badge and the preview bounds, wait several seconds, and confirm the preview remains open with the typed text intact and the caret still active.

**Acceptance Scenarios**:

1. **Given** the preview is in "adding note" state, **When** the user moves the pointer outside both the workspace badge hit-rect and the preview bounds, **Then** the preview does not close and the editor retains its draft text and caret position.
2. **Given** the preview is in "adding note" state, **When** the user clicks an empty cell of the underlying terminal (not on the preview, not on the badge), **Then** the click is intercepted as a dismiss attempt for the editor (see scenario 3) rather than passing through to the PTY.
3. **Given** the preview is in "adding note" state and the user has typed nothing, **When** a dismiss attempt occurs (e.g., a click outside the preview), **Then** the editor exits cleanly back to the read-only hover state without sending a mutation.
4. **Given** the preview is in "adding note" state and the user has typed non-empty text, **When** a dismiss attempt occurs, **Then** the draft is preserved through the existing workspace-notes draft pipeline so the text can be recovered when the modal is next opened for that workspace.
5. **Given** the preview is in "adding note" state, **When** a higher-priority overlay opens (notes modal, context menu, command palette, search overlay, close dialog, update dialog), **Then** the editor exits and any non-empty draft is flushed through the existing `SaveDraft` debounce pipeline before the higher overlay takes focus.

---

### User Story 3 - Discoverable Cancel and Abandon-Empty Semantics (Priority: P3)

A Scribe user can back out of an accidental "+" click or an abandoned draft without leaving residue in the notes list or the workspace draft buffer.

**Why this priority**: Once capture and dismiss-suppression work, this rounds out the workflow so the affordance is safe to explore. It also covers the empty/whitespace edge case that already exists in the modal path and must remain consistent.

**Independent Test**: Click "+" without typing, press the cancel key, confirm the preview returns to read-only with no entry created and no workspace draft set. Repeat after typing whitespace-only text. Repeat after typing real text followed by the cancel key, and confirm the cancel discards the text without creating an entry.

**Acceptance Scenarios**:

1. **Given** the preview is in "adding note" state with an empty draft, **When** the user presses Escape, **Then** the preview returns to its read-only state, no `CreateActiveNote` mutation is sent, and the workspace draft buffer is not changed.
2. **Given** the preview is in "adding note" state with whitespace-only draft text, **When** the user presses Enter, **Then** no entry is created, no mutation is sent, and the preview returns to read-only state (mirrors existing modal behavior for empty/whitespace drafts).
3. **Given** the preview is in "adding note" state with non-empty draft text, **When** the user presses Escape, **Then** the typed text is discarded, no mutation is sent, no workspace draft is written, and the preview returns to read-only state.
4. **Given** the preview is in "adding note" state and the server rejects the `CreateActiveNote` mutation, **When** the error broadcast arrives, **Then** the editor row preserves the typed text, surfaces a retryable error message in the same surface area the modal uses for note errors, and stays in "adding note" state so the user can retry without re-typing.

---

### Edge Cases

- **Adding-note state while workspace has zero active notes** — the preview already shows "No active notes"; the "+" affordance MUST be visible and clickable so the empty state is the natural entry point for capture.
- **Editor grows toward the pane's vertical limit** — the preview MUST grow dynamically with the editor row count up to **3/4 of the focused pane's height**; past that cap the editor row scrolls internally so the caret stays visible and the preview does not dominate the terminal. The preview MUST still reflow within the viewport (flip above/below the badge) as it already does for read-only previews when the anchor is near a viewport edge.
- **Modal editor spacebar bug** — the current modal editor silently drops spacebar keypresses, leaving notes with no spaces; this feature fixes that defect as part of the modal-keymap alignment work (see FR-018).
- **Concurrent click on an existing note row while editor is open** — clicks on existing read-only rows MUST continue to archive them as `Done`, but the click MUST NOT cancel the in-progress editor; archival of a row is independent of the editor's lifecycle.
- **Click on the editor row's body itself** — must focus / keep focus on the editor and MUST NOT archive anything.
- **Workspace switch (focused workspace changes) while editing** — exit "adding note" state, flush the non-empty draft via `SaveDraft`, and apply the normal hover-preview policy for the new workspace.
- **Window close / app shutdown while editing** — must be subject to the same draft-flush guarantee the modal editor already enforces; no quit-time loss of typed text.
- **Late `WorkspaceNotesChanged` snapshot arrives during editing** — the local draft must not be overwritten; the existing modal's "snapshot drafts hydrate only while local draft is pristine" rule applies to the inline editor as well.
- **Repeated commits / double-click on commit key** — only one `CreateActiveNote` mutation is in flight per draft; subsequent commits while the first is unacknowledged are coalesced or ignored until the server broadcast resolves.
- **Pointer hovering an existing row while editor is open** — hover highlight on existing rows continues to work; this does not change the editor's state.
- **Reduced-motion or no-animation environment** — affordance transitions (idle → hover → pressed, read-only → editing → committed) must remain readable even if the implementation uses no animations.
- **Terminal redraw / resize while editor is open** — the editor anchor follows the workspace badge's new position without losing the draft or caret state.
- **Pointer leaves badge + preview entirely, then returns later** — pointer-return to the same workspace's badge re-renders the editor with the preserved draft text, caret position, and any pending server error intact (per FR-003 + FR-021).
- **User clicks a *different* workspace's badge while editor A is open** — editor A's "adding note" state is preserved on workspace A; the other workspace's notes modal opens. After dismissing that modal, hovering workspace A's badge restores A's editor with its full state.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST render a discoverable "+" affordance in the bottom-right corner of the workspace-notes hover preview whenever the preview is visible in its read-only state. The affordance is a **bordered cell ~2 columns wide × 1 row tall containing a centered "+" glyph**, using the preview's existing chrome border treatment so it reads unambiguously as a button (not decoration).
- **FR-002**: Activating the "+" affordance (mouse click) MUST transition the preview into "adding note" state with an inline editor row positioned at the bottom of the note list, **pre-populated with the workspace's existing saved `SaveDraft` text (empty if none), caret placed at the end of that text**, and MUST hide or disable the "+" affordance for the duration of that state so a second concurrent draft cannot be initiated.
- **FR-003**: While a workspace is in "adding note" state, its preview MUST suppress the standard pointer-leave auto-close that the read-only hover preview uses. Pointer movement that takes the pointer outside the badge and preview bounds without entering another workspace's badge MUST leave the workspace's editor state intact; pointer-return to that workspace's badge MUST re-render the editor with its existing draft text, caret position, and any pending server error.
- **FR-004**: The inline editor MUST visually grow vertically as the draft text wraps or contains explicit newlines; the preview MUST reflow within the viewport clamp so it never extends off-screen, and the preview's outer height MUST stay within the cap defined by FR-019.
- **FR-005**: While in "adding note" state, keyboard input directed at the editor MUST be captured by the hover preview (consumed before PTY translation, mirroring the existing modal keyboard policy) and MUST NOT leak to the underlying terminal session.
- **FR-006**: Pressing Enter while the inline editor's draft is non-empty (after trimming whitespace) MUST send a `WorkspaceNotesMutation::CreateActiveNote` via the existing client-server pipeline; the preview MUST transition back to read-only state and reflect the new entry once the server broadcasts the updated collection. Because the inline editor and the modal share one draft buffer (FR-020), this commit MUST leave the shared `SaveDraft` empty afterward — matching the modal's existing post-commit state — so the buffer is not re-hydrated with stale text on the next open of either editor.
- **FR-007**: Pressing Enter while the inline editor's draft is empty or whitespace-only MUST NOT send a mutation; the preview MUST return to its read-only state without altering the workspace's notes or draft buffer.
- **FR-008**: Pressing Escape MUST discard the in-progress draft text, return the preview to read-only state, and MUST NOT write to the workspace's notes list or draft buffer.
- **FR-009**: Pressing Ctrl+Enter MUST insert a literal newline into the inline editor's draft at the caret position (matches the in-terminal Ctrl+Enter convention used elsewhere in Scribe).
- **FR-010**: When a dismiss attempt arrives via a higher-priority overlay (notes modal, context menu, command palette, search overlay, close dialog, update dialog) while the editor is open, the editor MUST exit and any non-empty draft MUST be flushed through the existing `SaveDraft` debounce path before the higher overlay takes focus.
- **FR-011**: Clicks on the existing read-only note rows in the preview MUST continue to send `ArchiveNote { reason: Done }` regardless of whether the editor is open; clicks on the inline editor row MUST NOT archive any existing note.
- **FR-012**: If the server returns an error in response to the `CreateActiveNote` mutation, the editor MUST preserve the typed text, surface a retryable error message in the same chrome the modal uses for server errors, and remain in "adding note" state until the user retries or cancels.
- **FR-013**: A change in the focused workspace (without opening that workspace's notes modal) MUST NOT exit any inline editor's "adding note" state. Each workspace's editor is tied to the workspace itself (FR-021), not to focus, so a user can navigate between panes / focused workspaces without disturbing an inline editor that belongs to a different workspace. Opening the notes modal for the workspace whose editor is currently open remains covered by FR-010 — that path exits the editor and flushes the draft.
- **FR-014**: Window close, application shutdown, and update-relaunch transitions MUST defer until any in-flight inline-editor draft has been flushed through `SaveDraft` (matching the existing modal flush guarantee).
- **FR-015**: A late `WorkspaceNotesChanged` snapshot arriving while the inline editor has a non-empty dirty draft MUST NOT overwrite the draft text (mirrors the modal's pristine-draft policy).
- **FR-016**: Successive commits during an in-flight `CreateActiveNote` MUST NOT send duplicate mutations; the editor MUST coalesce or ignore the additional commit until the prior server broadcast resolves.
- **FR-017**: The existing workspace-notes modal editor (opened by clicking the workspace badge) MUST be updated to use the same keymap as the inline editor: **Enter commits the active draft, Ctrl+Enter inserts a literal newline at the caret, and Escape closes or cancels edits.** This replaces the modal's prior Ctrl+Enter-saves / Enter-inserts-newline binding so users see one consistent editor across modal and hover-preview entry points.
- **FR-018**: The workspace-notes modal editor MUST insert a single space character into the active draft when the user presses the spacebar. (Fixes a pre-existing defect where spacebar input is silently dropped in the modal editor.)
- **FR-019**: The preview's outer height in "adding note" state MUST grow dynamically with the editor's row count up to **3/4 of the focused pane's vertical extent**. Past that cap, the editor row MUST scroll internally so the caret stays visible while the preview's outer height stays clamped at 3/4 of the pane. The cap MUST track pane resize so the limit always reflects the current pane height.
- **FR-020**: The inline editor and the modal's New-note editor MUST share a **single saved-draft buffer per workspace**. Opening either entry point MUST pre-populate the editor with that buffer's current text (FR-002); typing in either editor MUST write back to the same buffer through the `SaveDraft` debounce; committing via Enter from either editor MUST consume and clear the buffer (FR-006). Late `WorkspaceNotesChanged` snapshots that include a saved draft MUST NOT overwrite the buffer while either editor holds a dirty local copy (the existing pristine-draft policy from `004-workspace-notes`).
- **FR-021**: The inline editor's "adding note" state MUST be **scoped per workspace** (parallel to the per-workspace `SaveDraft` buffer in FR-020). Multiple workspaces can independently hold "adding note" state simultaneously — relevant when split panes show more than one workspace badge, or when the user moves between workspace badges without committing/cancelling each editor. Hovering a different workspace's badge MUST render that workspace's own preview state (read-only or editor) without affecting any other workspace's "adding note" state.
- **FR-022**: When the inline editor's draft exceeds the visible row count permitted by FR-019, the editor MUST support **three scroll inputs** working in concert:
  - **Caret-tracking auto-scroll** — typing or moving the caret (arrow keys, Home/End, etc.) MUST scroll the editor's internal viewport so the caret stays visible.
  - **Mouse-wheel scrolling over the editor row** MUST scroll the editor's internal viewport without moving the caret; resuming typing snaps the viewport back to the caret. Wheel events outside the editor row MUST NOT be consumed by the editor (they pass through to the rest of the preview / terminal beneath, matching the read-only preview's existing wheel behavior).
  - **Overlay scrollbar inside the editor row** following Scribe's existing terminal-scrollback `ScrollbarState` pattern (`crates/scribe-client/src/scrollbar.rs`): fades in on scroll, fades out after 1.5 s of inactivity (0.3 s fade-out duration), hover-expands width via lerp, hit zone is 3× the visible width for reliable targeting, and drag-to-scroll computes the offset from mouse delta relative to track height. The scrollbar MUST be local to the editor row — it does not interact with or render alongside the terminal-scrollback scrollbar.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST extend the existing `WorkspaceNotesPreview` GPU-rendered overlay and the existing `WorkspaceNotesMutation` protocol; no new server-side persistence path, file format, or RPC variant is required.
- **QR-002**: Each user story's independent verification path MUST be executable from `quickstart.md`; new automated tests MUST be requested explicitly in the implementation plan or deferred to manual quickstart with a written rationale.
- **UX-001**: The inline editor's commit key (Enter), newline key (Ctrl+Enter), cancel key (Escape), empty-draft suppression, server-error display, and draft-preservation policy MUST be visibly consistent with the `WorkspaceNotesModal` editor's keymap as updated by FR-017 and FR-018, so users encounter one mental model across modal and hover-preview entry points. Documentation, settings UI, and any in-app help that names the modal's commit binding MUST be updated to the new keymap as part of this feature.
- **UX-002**: The "+" affordance MUST be unambiguously interactive — rendered as a **~2-column × 1-row bordered cell containing a centered "+" glyph** (see FR-001), with distinct idle, hover, pressed (active), and disabled visual states drawn from the `ChromeColors` palette (same border treatment as the preview's outer chrome). The click target MUST be the **full bordered-cell rect** (not just the glyph), with ≥ 1 cell of clear space between the affordance and the nearest note row above it and ≥ 1 cell of inset from the preview's right and bottom inner borders so the affordance reads as distinct from the note list.
- **UX-003**: Pointer-leave dismiss suppression MUST apply only while the inline editor is in "adding note" state; entering "adding note" state from a benign hover MUST NOT introduce a sticky preview that survives after the user explicitly commits or cancels.
- **UX-004**: When the editor row contains multi-line text, line wrapping and explicit newlines MUST keep the entire draft visible without clipping; the visual treatment (caret, padding, background contrast) MUST clearly distinguish the editor row from read-only note rows so users see the state change.
- **PR-001**: The transition from "+" click to caret-ready editor MUST complete within a single render tick at the project's target frame rate (no perceptible lag); growing the editor row as text wraps MUST NOT cause measurable stutter at typical note lengths (up to the preview's row budget).
- **PR-002**: Inline draft typing MUST piggy-back on the existing `SaveDraft` debounce pipeline so per-keystroke server bandwidth stays bounded by the existing workspace-notes debounce window, not by raw keystroke frequency.
- **PR-003**: The feature MUST NOT introduce new long-lived allocations or per-frame work in the read-only hover path for users who never engage the "+" affordance.

### Key Entities

This feature does not introduce any new persistent entities. It reuses:

- **`WorkspaceNotesMutation::CreateActiveNote`** — existing protocol variant used by the modal's New-note editor.
- **`WorkspaceNotesMutation::SaveDraft`** — existing debounced draft path used to preserve in-progress text across overlays, workspace switches, and shutdown.
- **`WorkspaceNoteSummary`** — existing read-only projection the preview already consumes; new entries appear here after the server broadcasts the updated collection.

The only new state is transient client UI state held by the preview: an "adding note" mode with draft text, dirty flag, caret position, and the last server error string for that draft attempt.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user can capture a new active note from the hover preview in zero modal opens (no modal-open call needed for the happy path), reducing the steps for the most common "capture a quick note" flow.
- **SC-002**: 100% of in-progress drafts survive deliberate pointer movement outside both the workspace badge and the preview bounds for an arbitrary duration; the preview only exits "adding note" state via explicit commit, explicit cancel, higher-priority overlay handoff, workspace switch, or app shutdown.
- **SC-003**: For users who never click the "+" affordance, the hover preview's read-only behavior (rows, click-to-archive, hover highlight, draft-preserving close, anchor reflow) is observably unchanged from the `004-workspace-notes` baseline.
- **SC-004**: New entries created through the inline editor are visible in the preview's list immediately after the server broadcasts the mutation, with no client-side refresh action required and no additional server round-trips beyond the existing `CreateActiveNote` flow.
- **SC-005**: No new persistent files or server-state shape changes are introduced; cold restart of a client that never used the inline affordance behaves identically to the `004-workspace-notes` baseline.
- **SC-006**: Discoverability — a Scribe user who has never used the feature, on first hover of a workspace badge, identifies the "+" affordance as the way to add a note without external instruction (verified by the affordance being visible, distinct, and positioned consistently in the preview surface).
- **SC-007**: Editor parity — the modal editor (opened from the workspace badge) and the inline editor (opened from the hover preview's "+" affordance) accept exactly the same key bindings for commit, newline, and cancel after this feature ships; spacebar reliably inserts a single space character in both editors.

## Assumptions

- The "+" affordance is rendered inside the existing preview surface (not as a separate floating element over the workspace badge) so the click target lives in the surface the user is already interacting with.
- The inline editor reuses the existing `WorkspaceNotesMutation` protocol path: `SaveDraft` for debounced in-progress text, `CreateActiveNote` for commit. No new protocol message is introduced.
- The "3/4 of the pane's vertical extent" cap in FR-019 is measured against the **focused workspace's terminal pane** (the same coordinate space the preview's existing `viewport` clamp uses); for windows with multiple split panes, "the pane" is whichever pane currently owns the focused workspace badge.
- Higher-priority overlays (notes modal, context menu, command palette, search overlay, close dialog, update dialog) take precedence over the inline editor — opening any of them exits "adding note" state and flushes any non-empty draft via `SaveDraft`.
- The hover preview's existing draft-pristine policy (snapshots only hydrate a pristine local draft) extends to the inline editor unchanged.
- The "click an existing note row to archive as Done" gesture remains independent of the inline editor's lifecycle; archival can happen while the editor is open and does not cancel the edit.
- The feature is desktop-only (Scribe's existing target surface); no mobile / touch / accessibility-tooling adaptations are introduced beyond what the project already supports.
- The modal keymap change in FR-017 is a deliberate breaking change for existing users; mental-model consistency with the new inline editor is judged more valuable than backward compatibility with the prior Ctrl+Enter-saves binding. No "legacy keymap" toggle is offered.
- The Enter / Ctrl+Enter / Escape bindings for both the inline editor and the modal editor (FR-006 / FR-008 / FR-009 / FR-017) are **hardcoded** — they are NOT exposed through `KeybindingsConfig` and do NOT appear in the Settings keybindings page. This matches the existing project pattern (modal-internal editor keys are not in `KeybindingsConfig` today; that surface only exposes top-level app actions like pane navigation, splits, clipboard, etc.). Configurability can be added later as a non-breaking enhancement if users request it.

## Clarifications

### Session 2026-05-19

- Q: Which key binding should commit the inline draft? → A: **Enter commits, Ctrl+Enter inserts a newline.** This matches the in-terminal Ctrl+Enter convention used elsewhere in Scribe. The existing modal editor MUST also be updated to this same binding (see FR-017) so the two editors share one keymap.
- Q: When the user presses Escape with a non-empty draft, what should happen? → A: **Discard on Escape** (explicit cancel = throw away, matches modal Escape semantics). Indirect dismissal paths (higher-priority overlay handoff, workspace switch, shutdown) still flush the non-empty draft through `SaveDraft` so the text is recoverable from the modal. Two intents → two behaviors.
- Q: What happens when the draft exceeds the preview's existing vertical row budget? → A: **Dynamic growth up to 3/4 of the focused pane's height; then scroll internally.** The preview's outer height is capped at 3/4 of the pane (not at the read-only `MAX_PREVIEW_ROWS = 12` constant); past that cap, the editor row scrolls so the caret stays visible. See FR-019.
- Bug surfaced during clarification: the modal editor silently drops spacebar input; fix as part of this feature's modal-keymap alignment work. See FR-018.
- Q: When the inline editor opens, should it pre-populate with the workspace's existing saved draft or start blank? → A: **Pre-populate from the workspace's saved `SaveDraft` value.** The modal's New-note editor and the inline editor are two views onto **one shared draft buffer per workspace** — opening either path shows the same in-progress text, typing in either writes back to the same buffer through `SaveDraft`, and committing via Enter from either path consumes the buffer (see FR-020).
- Q: Can multiple workspace previews each be in "adding note" state simultaneously, or is the inline editor a singleton? → A: **Per workspace.** Each workspace independently owns its "adding note" state (FR-021); editors are not closed by pointer-movement to a different workspace's badge, by focus changes, or by opening a different workspace's modal. The state persists across hover gaps and is restored on pointer-return. Singletons only kick in for app-wide higher-priority overlays (context menu, command palette, search overlay, close dialog, update dialog) — which still close all hover previews and flush all open editors' drafts per FR-010.
- Q: When the inline editor scrolls internally (past the 3/4-pane cap), what gesture moves the visible portion? → A: **Three scroll inputs (FR-022).** Caret-tracking auto-scroll (typing/arrow keys), mouse-wheel scrolling inside the editor row (without moving the caret), and an overlay scrollbar following the existing terminal-scrollback `ScrollbarState` pattern (`crates/scribe-client/src/scrollbar.rs`: 1.5 s idle fade, hover-expand width, 3× hit zone, drag-to-scroll).
- Q: Should the notes-editor keymap (Enter / Ctrl+Enter / Escape) be configurable via the Settings keybindings page, or hardcoded? → A: **Hardcoded.** The bindings live in the same hardcoded modal-internal layer as the existing modal's editor keys; they are NOT exposed in `KeybindingsConfig` or the Settings keybindings page. Matches existing project pattern; configurability is a deferred non-breaking enhancement.
- Q: What's the visual treatment of the "+" affordance? → A: **Bordered "+" cell, ~2 columns × 1 row** (FR-001, UX-002) — a small button-like control using the preview's existing chrome border treatment with distinct idle / hover / pressed / disabled states from `ChromeColors`. Click target is the full bordered-cell rect. Chosen over a pure glyph (ambiguous as interactive) and a labeled button (consumes horizontal space the preview can't easily spare).
