# Contract: Workspace Notes UI

## Purpose

Defines the user-facing interaction contract for opening, editing, archiving,
previewing, and synchronizing server-backed per-workspace notes in Scribe's
client chrome.

## Entry Points

### Workspace Tab Click

Clicking the workspace badge/name area opens the notes modal for that workspace.

Rules:
- Terminal tab click, drag, close, and equalize controls keep existing behavior.
- If multiple workspaces are visible, the modal opens centered over the clicked
  workspace's pane area.
- If the clicked workspace is not focused, opening notes may focus that
  workspace, but it must not send input to any terminal session.
- Modal content is read from the latest server-backed client cache; cache misses
  request a server snapshot.

### Workspace Tab Hover

Hovering the workspace badge/name area displays a server-backed active-note
preview.

Rules:
- Preview appears without changing keyboard focus.
- Preview includes active notes only.
- Preview stays visible while the pointer is over the workspace badge or
  preview bounds.
- Visible preview rows highlight on hover.
- Clicking a visible preview row sends an archive-as-done mutation.
- Preview disappears when the pointer leaves the badge/preview region or when
  the modal opens.

## Modal Layout

The modal is centered inside the selected workspace's pane area.

Required regions:
- Header with compact workspace context.
- Active note list or archive list.
- Multi-line editor.
- Minimal controls for save, edit, done/remove, archive navigation, and close.

Rules:
- The terminal context remains visible around the modal.
- Long lists scroll inside the modal rather than resizing the workspace.
- Empty state for a workspace with no notes keeps the editor focused.
- UI copy stays terse and task-oriented.
- Incoming server broadcasts update visible lists without closing the modal.

## Keyboard Contract

While the modal is open:
- Typing inserts text into the editor.
- Enter inserts a newline.
- Ctrl+Enter sends the current non-empty draft or edit as a server mutation.
- Escape closes the modal only after draft text has been synced or the user has
  explicitly discarded it.
- Navigation keys move within the modal list or editor.

Rules:
- Modal keyboard events are consumed before terminal key translation.
- Keystrokes must not reach the PTY while the modal owns focus.
- Draft text is sent to the server with a debounce during typing.
- Modal close, workspace switch, shutdown, and Ctrl+Enter save force a final
  draft sync before local editor text is discarded.

## Active Note Management

Users can edit and archive active notes from the modal.

Rules:
- Editing an active note keeps it active.
- Done and remove actions both move notes to archive.
- After archiving the last active note, the active list is empty and hover
  preview no longer shows that note.
- If a server mutation fails, the client keeps local editor text and surfaces a
  compact retryable error state.

## Archive Management

Users can enter an archive view from the modal.

Rules:
- Archive view displays archived notes only.
- Editing one archived note keeps it archived.
- Edit-all mode lets users update multiple archived notes before one save.
- Archive edits do not modify active notes.
- Restore/reactivate is out of v1 scope unless a later spec adds it.

## Hover Preview Layout

The preview is a compact list anchored near the workspace tab/badge.

Rules:
- Show all active entries when space allows.
- For long or many notes, show bounded visible summaries and an overflow count
  while leaving full content available in the modal.
- Preview must not cover more terminal content than necessary.
- Preview should render within 100 ms for up to 50 active notes from cached
  server state.

## Multi-Window Sync

All connected clients receive `WorkspaceNotesChanged` broadcasts.

Rules:
- Visible modal lists and hover previews update from broadcasts.
- If another client edits the same draft or note, the last server-received
  mutation wins and the current client updates to the broadcast state.
- The client may keep an active local editing buffer until it receives a
  broadcast for the same target; after that, it must avoid showing stale saved
  state as authoritative.

## Layering and Focus

Ordering from highest to lowest priority:
1. Close/update dialogs and context menu.
2. Workspace notes modal.
3. Command palette and search overlay.
4. Workspace notes hover preview and tooltips.
5. Terminal content and tab chrome.

Rules:
- Modal suppresses hover preview for the same workspace.
- Hover preview is subordinate to existing dialogs and context menus.
- Existing close/update dialogs continue to intercept input first.
