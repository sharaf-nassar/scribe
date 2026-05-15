# Feature Specification: Workspace Notes

**Feature Branch**: `004-workspace-notes`  
**Created**: 2026-05-15  
**Status**: Draft  
**Input**: User description: "Build a way to add notes per workspace. When the workspace tab is clicked, open a centered modal in the pane for adding, editing, and removing note entries. The cursor should be ready to type when the modal opens. Enter adds a new line, Ctrl+Enter saves the note as an entry. Removed or done notes should be archived. Users should be able to navigate to archived notes from the modal to edit one note or all archived notes. Hovering over the workspace tab should instantly display a minimal list of all active entries."

## Clarifications

### Session 2026-05-15

- Q: Where should workspace notes authoritative state live? → A: Server is authoritative: notes/drafts persist in server state, clients send mutations, server broadcasts updates.
- Q: How should the new server-backed implementation handle notes already saved by the current client-only implementation? → A: Start fresh with server-backed notes and ignore old client-local note files.
- Q: When should unsaved draft text be persisted to the server? → A: Debounced server draft updates while typing, plus immediate flush on modal close, workspace switch, and shutdown.
- Q: How should conflicting note or draft edits from multiple client windows be resolved? → A: Last server-received mutation wins; server broadcasts the winning state to all clients.
- Q: What durability guarantee should the server provide for accepted note mutations? → A: Write-through: persist each accepted mutation before ack/broadcast.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Capture Workspace Notes From the Tab (Priority: P1)

A Scribe user can click a workspace tab and immediately capture a note for that workspace without leaving the terminal context.

**Why this priority**: This is the core workflow. The feature is valuable only if note capture is fast, local to the workspace, and keyboard-friendly.

**Independent Test**: Can be fully tested by clicking a workspace tab, typing a multi-line note, saving it with Ctrl+Enter, and confirming the saved entry appears as an active note for that workspace only.

**Acceptance Scenarios**:

1. **Given** a workspace tab is visible, **When** the user clicks the workspace tab, **Then** a notes modal opens centered over that workspace's pane area.
2. **Given** the notes modal has opened, **When** the user starts typing, **Then** text appears in the note editor without requiring an additional click.
3. **Given** the user is typing in the note editor, **When** the user presses Enter, **Then** a newline is inserted into the draft note.
4. **Given** the user has typed a non-empty draft note, **When** the user presses Ctrl+Enter, **Then** the draft is saved as an active note entry for that workspace.
5. **Given** the user switches to a different workspace, **When** that workspace's notes modal is opened, **Then** notes from the previous workspace are not shown as active entries for the new workspace.

---

### User Story 2 - Manage Active Notes (Priority: P2)

A Scribe user can review, edit, and mark active workspace notes as done from the same modal.

**Why this priority**: Note capture without upkeep creates stale entries. Users need a low-friction way to keep the active list current.

**Independent Test**: Can be fully tested by creating multiple active notes, editing one, marking one done, and confirming the done note leaves the active list while the edited note remains active.

**Acceptance Scenarios**:

1. **Given** a workspace has active notes, **When** the user opens that workspace's notes modal, **Then** active notes are listed with controls for editing and marking them done or removed.
2. **Given** an active note is being edited, **When** the user saves the edit, **Then** the active note reflects the updated content and remains attached to the same workspace.
3. **Given** the user marks an active note done or removes it, **When** the action completes, **Then** the note is moved to that workspace's archive instead of being permanently deleted.

---

### User Story 3 - Review and Edit Archived Notes (Priority: P3)

A Scribe user can navigate from the notes modal to archived notes and revise archived content when old context becomes useful again.

**Why this priority**: Archiving preserves workspace context while keeping the active list focused. Archive access is secondary to fast capture and active-note management.

**Independent Test**: Can be fully tested by archiving notes, opening the archive view from the modal, editing a single archived note, and using an edit-all mode to update multiple archived notes before saving.

**Acceptance Scenarios**:

1. **Given** a workspace has archived notes, **When** the user opens the archive view from the modal, **Then** archived notes for that workspace are shown separately from active notes.
2. **Given** an archived note is visible, **When** the user edits and saves it, **Then** the archived note keeps its archived status and displays the updated content.
3. **Given** the user chooses to edit all archived notes, **When** changes are saved, **Then** all modified archived notes are updated together without changing active notes.

---

### User Story 4 - Preview Active Notes on Hover (Priority: P4)

A Scribe user can hover a workspace tab and instantly see a minimal active-note preview without opening the modal.

**Why this priority**: Hover preview makes workspace context glanceable, but the modal workflows still provide the main capture and management value.

**Independent Test**: Can be fully tested by creating active notes, hovering the workspace tab, confirming the active-note list appears quickly, and confirming archived notes are excluded.

**Acceptance Scenarios**:

1. **Given** a workspace has active notes, **When** the user hovers its workspace tab, **Then** a minimal list of that workspace's active entries appears near the tab without opening the modal.
2. **Given** a workspace has only archived notes, **When** the user hovers its workspace tab, **Then** the preview indicates there are no active entries or remains unobtrusive.
3. **Given** a workspace has long or multi-line active notes, **When** the hover preview appears, **Then** entries remain readable without covering the terminal pane more than necessary.

### Edge Cases

- Opening the modal for a workspace with no notes should show an empty active state and focus the editor.
- Saving an empty or whitespace-only draft should not create an entry.
- Closing the modal with an unsaved draft should warn the user or preserve the draft until they explicitly discard it.
- Draft text should be sent to the server during typing with a debounce, and should be flushed immediately on modal close, workspace switch, and shutdown.
- Long, multi-line notes should remain editable and previewable without breaking tab layout or covering unrelated workspaces.
- A workspace with many active or archived notes should remain navigable through search, filtering, or compact scrolling.
- Archiving the last active note should clear the active list and remove it from the hover preview.
- Hover preview should not steal keyboard focus from the active terminal.
- Modal keyboard shortcuts should apply only while the notes modal is active and must not leak into the terminal session.
- If multiple client windows edit the same note or draft, the last mutation received by the server should become authoritative and all clients should update to that state.
- If the server cannot persist an accepted note mutation, it should not acknowledge or broadcast that mutation as applied.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST associate note entries with exactly one workspace.
- **FR-002**: Users MUST be able to open a workspace notes modal by clicking that workspace's tab.
- **FR-003**: The notes modal MUST open centered over the pane area associated with the selected workspace.
- **FR-004**: The note editor MUST receive text focus automatically when the modal opens.
- **FR-005**: While editing a note draft, Enter MUST insert a newline.
- **FR-006**: While editing a note draft, Ctrl+Enter MUST save the current non-empty draft as a note entry.
- **FR-007**: Users MUST be able to view all active note entries for the selected workspace in the modal.
- **FR-008**: Users MUST be able to edit active note entries and save changes.
- **FR-009**: Users MUST be able to mark active note entries as done or remove them from the active list.
- **FR-010**: Notes marked done or removed MUST move to the selected workspace's archive instead of being permanently deleted.
- **FR-011**: Users MUST be able to navigate from the notes modal to archived notes for the selected workspace.
- **FR-012**: Users MUST be able to edit a single archived note without changing its archived status.
- **FR-013**: Users MUST be able to edit multiple archived notes in one archive-management flow.
- **FR-014**: Archived notes MUST remain excluded from the active-note list unless the user explicitly restores or reactivates them in a future workflow.
- **FR-015**: Hovering a workspace tab MUST display a minimal preview of all active note entries for that workspace.
- **FR-016**: The hover preview MUST exclude archived notes.
- **FR-017**: Notes, note edits, and archived status MUST remain available after closing and reopening Scribe unless the associated workspace is intentionally removed.
- **FR-018**: The system MUST prevent accidental loss of unsaved note drafts when a user closes the modal, switches workspace, or dismisses the overlay.
- **FR-019**: The modal and hover preview MUST remain scoped to the workspace tab being clicked or hovered, even when multiple workspace panes are visible.
- **FR-020**: User-facing labels and states MUST use compact, work-focused language suitable for a terminal workflow.
- **FR-021**: The server MUST be the authoritative owner and persistent store for workspace note collections, including unsaved drafts.
- **FR-022**: Clients MUST send workspace note mutation requests to the server and render note state from server responses or broadcasts.
- **FR-023**: The server MUST broadcast workspace note changes to all connected clients so multi-window note views remain consistent.
- **FR-024**: The server-backed implementation MUST start with its own note store and MUST NOT automatically import existing client-local `workspace_notes.toml` files.
- **FR-025**: Clients MUST send debounced draft updates to the server while the user types and MUST force a final draft sync on modal close, workspace switch, and shutdown.
- **FR-026**: When multiple clients mutate the same workspace note or draft, the server MUST treat the last received mutation as authoritative and broadcast the resulting state to all clients.
- **FR-027**: The server MUST durably persist each accepted workspace note mutation before acknowledging it to the requesting client or broadcasting it to other clients.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture boundaries and use existing project abstractions unless the plan states why a divergence is required.
- **QR-002**: New test code is not requested by this specification. Planning MUST define manual quickstart verification and may propose focused automated tests only if the user explicitly approves them later.
- **UX-001**: The notes experience MUST feel consistent with Scribe's terminal workspace model, including focus behavior, tab interactions, keyboard shortcuts, and pane-centered overlays.
- **UX-002**: The modal MUST keep the terminal context visible enough that users understand which workspace the notes belong to.
- **UX-003**: The hover preview MUST be minimalistic, fast to scan, and visually subordinate to the workspace tab and terminal content.
- **UX-004**: The modal MUST avoid decorative labels, oversized marketing-style copy, and interaction patterns that feel disconnected from a professional terminal.
- **PR-001**: Opening the notes modal after clicking a workspace tab SHOULD feel immediate to the user, with the editor ready for typing within 150 ms on typical Scribe workspaces.
- **PR-002**: Hover preview SHOULD appear within 100 ms for workspaces with up to 50 active notes.
- **PR-003**: Managing notes SHOULD preserve smooth terminal rendering and avoid noticeable frame drops during modal open, close, hover, edit, and archive transitions.

### Key Entities

- **Workspace Notes Collection**: The server-owned note set for one workspace. It contains active notes, archived notes, and any unsaved draft associated with that workspace.
- **Note Entry**: A single user-authored note. Key attributes include workspace association, text content, active or archived status, creation time, last edited time, and archived time when applicable.
- **Draft Note**: Server-persisted unsaved text currently being edited in a workspace notes modal. It is updated during typing with a debounce, flushed at lifecycle boundaries, and is not an active note entry until saved.
- **Hover Preview**: A transient read-only display of the selected workspace's active note entries.
- **Archive View**: A modal state that shows archived note entries separately from active entries and supports single-note and multi-note editing.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 95% of users can create and save a first workspace note from the tab in under 10 seconds without reading instructions.
- **SC-002**: 95% of modal openings place the typing cursor in the note editor immediately, with no extra click required.
- **SC-003**: 100% of notes marked done or removed remain recoverable from the same workspace's archive.
- **SC-004**: 95% of hover previews appear in under 100 ms for workspaces with 50 or fewer active notes.
- **SC-005**: Users can distinguish active notes from archived notes with no ambiguity during manual verification.
- **SC-006**: Notes created in one workspace do not appear in another workspace's active list or archive during cross-workspace verification.
- **SC-007**: A user can edit one archived note and save bulk edits to multiple archived notes within the archive view without affecting active notes.
- **SC-008**: When two client windows edit the same workspace note or draft, all connected clients converge on the last mutation received by the server.
- **SC-009**: After a note mutation is acknowledged or broadcast, restarting or updating Scribe preserves that mutation in the server-backed note store.

## Assumptions

- Workspace notes are intended to be server-owned durable workspace context, not temporary session-only scratch text.
- Existing client-local note files from the earlier implementation are ignored by the server-backed feature.
- "Removed" and "done" mean the same archival outcome for this feature: the note leaves the active list and enters the workspace archive.
- Archived notes stay archived after editing unless a later feature adds explicit restore or reactivate behavior.
- The hover preview is read-only; note editing happens in the modal.
- The feature targets desktop Scribe terminal usage with keyboard and pointer input.
- No automated test code is requested in this specification phase; verification details can be refined during planning.
