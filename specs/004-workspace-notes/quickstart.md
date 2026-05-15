# Quickstart: Workspace Notes Verification

This quickstart describes manual verification for the server-backed Workspace
Notes feature. Do not restart the live Scribe server while running these checks
unless the user explicitly approves it.

## Prerequisites

- Build a development Scribe client/server pair in a way that does not disrupt
  the user's active server.
- Open a window with at least two workspaces visible.
- Give one workspace a visible name so its workspace badge/name is available in
  the tab bar.
- Start with a clean server-backed workspace notes store. Existing
  client-local `workspace_notes.toml` files should not be imported.

## Scenario 1: Capture a Workspace Note

1. Click the workspace badge/name in the tab bar.
2. Confirm the notes modal opens centered over that workspace's pane area.
3. Type `first line`, press Enter, then type `second line`.
4. Press Ctrl+Enter.
5. Confirm one active note appears for that workspace and preserves both lines.
6. Open a different workspace's notes modal.
7. Confirm the first workspace's note does not appear there.

Expected result: note capture works in under 10 seconds, the editor accepts
typing without an extra click, and the saved note came from a server mutation.

## Scenario 2: Manage Active Notes

1. Create two active notes in one workspace.
2. Edit the first active note and save it.
3. Mark the second active note done or removed.
4. Confirm the edited first note remains active.
5. Confirm the second note leaves the active list.

Expected result: active-note edits are preserved and done/remove archives notes
instead of deleting them.

## Scenario 3: Review Archived Notes

1. Open the archive view from the notes modal.
2. Confirm the previously archived note appears in the archive list.
3. Edit that archived note and save it.
4. Enter edit-all archive mode.
5. Modify multiple archived notes and save.
6. Return to the active view.

Expected result: archived notes remain archived after edits, bulk archive edits
do not affect active notes, and active/archived states stay visually distinct.

## Scenario 4: Hover Preview and Preview Clicks

1. Create several active notes in a workspace.
2. Move the pointer over the workspace badge/name.
3. Confirm a compact preview appears quickly near the tab.
4. Move the pointer into the preview and across note rows.
5. Confirm the preview remains open and visible rows highlight on hover.
6. Click a visible note row.
7. Hover again.

Expected result: preview shows active notes only, excludes archived notes,
stays reachable from the tab, highlights the row about to be clicked, archives
the clicked row as done, and appears within the 100 ms target for ordinary note
counts.

## Scenario 5: Draft Preservation

1. Open the notes modal and type a draft without saving.
2. Wait long enough for the draft debounce to send.
3. Close or dismiss the modal.
4. Reopen the same workspace's notes modal.
5. Confirm the draft is preserved.
6. Repeat with a workspace switch and with normal client shutdown.

Expected result: unsaved note text is not accidentally lost because draft state
was sent to and persisted by the server.

## Scenario 6: Multi-Window Convergence

1. Open two client windows connected to the same server.
2. Open the same workspace notes modal in both windows.
3. Edit the same draft or note in window A and save.
4. Edit the same draft or note in window B and save after A.
5. Inspect both windows.

Expected result: both windows converge on the last mutation received by the
server, and the server broadcasts the winning state to every connected client.

## Scenario 7: Persistence Across Restart or Update

1. Create active notes, archived notes, and a draft.
2. Ensure the note mutations have been acknowledged or broadcast.
3. With explicit user approval, restart/update the development Scribe server.
4. Reopen or reconnect the client.
5. Open the same workspace's notes modal and hover preview.

Expected result: active notes, archived notes, edits, and draft state remain
available from the server-backed store after restart/update.

## Scenario 8: Legacy Client-Local File Is Ignored

1. Place a valid old client-local `workspace_notes.toml` where the earlier
   client implementation would have read it.
2. Start the server-backed implementation with an empty server store.
3. Open the workspace notes modal.

Expected result: old client-local notes are not imported automatically. The
server-backed feature starts from its own store. Legacy files that lack the
server-owned `owner = "server"` marker are treated as non-authoritative.

## Performance Checks

- Modal editor is ready for typing within 150 ms of clicking the workspace
  badge/name.
- Hover preview appears within 100 ms with 50 active notes from cached server
  state.
- Opening, editing, archiving, syncing drafts, and hovering notes does not cause
  visible frame drops or delayed terminal repaint.

## Notes

- New automated test code is not requested by this feature spec. If later
  approved, prioritize focused tests for server state transitions, write-through
  persistence failure behavior, IPC mutation/broadcast flow, workspace scoping,
  conflict ordering, empty-note rejection, and archive filtering.
- Command verification can run without restarting the live server. Manual
  restart/update scenarios require a separate development server or explicit
  user approval to disturb the active Scribe environment.
