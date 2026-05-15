# Contract: Workspace Notes State

## Purpose

Defines the durable server-owned state shape and mutation behavior for
per-workspace notes. This is an internal server persistence contract, not a
settings schema and not a client-owned state file.

## Storage Location

Workspace notes are stored under the active Scribe state directory from the
server process:

```text
$XDG_STATE_HOME/<scribe-or-scribe-dev>/workspace_notes.toml
```

Existing client-local note files from the earlier implementation are ignored.
The server-backed feature starts with its own store and leaves old files
untouched.

If the state directory cannot be resolved or a write fails, the server rejects
the affected mutation and does not acknowledge or broadcast it as applied.

## State File Shape

```toml
version = 1
owner = "server"
updated_at_ms = 1778880000000

[workspaces."<workspace-id>"]
workspace_id = "<workspace-id>"
updated_at_ms = 1778880000000

[workspaces."<workspace-id>".draft]
workspace_id = "<workspace-id>"
text = "unfinished note"
updated_at_ms = 1778880000000
dirty = true

[[workspaces."<workspace-id>".active_notes]]
note_id = "<note-id>"
workspace_id = "<workspace-id>"
text = "ship workspace notes"
status = "active"
created_at_ms = 1778880000000
updated_at_ms = 1778880000000

[[workspaces."<workspace-id>".archived_notes]]
note_id = "<note-id>"
workspace_id = "<workspace-id>"
text = "old context"
status = "archived"
created_at_ms = 1778870000000
updated_at_ms = 1778880000000
archived_at_ms = 1778880000000
archive_reason = "done"
```

## Persistence Rules

- Server loads the store during startup before registered clients need note
  snapshots.
- Missing file returns an empty version-1 store.
- Files without `owner = "server"` return an empty store and log a warning,
  which makes legacy client-local note files non-importing by default.
- Parse errors return an empty store and log a warning; terminal startup
  continues.
- Unsupported future versions return an empty store and log a warning.
- Each accepted mutation is written atomically before server ack/broadcast.
- Persistence failure leaves the previous in-memory and on-disk note state
  authoritative and returns an error to the requester.
- No client writes note state to disk.

## Mutation Operations

### Save Draft

**Input**: `workspace_id`, `text`.  
**Output**: updated `WorkspaceNotesCollection`.

Rules:
- Save draft updates server state after successful write-through persistence.
- Dirty non-empty and whitespace-only text are both preserved as drafts.
- Last server-received draft mutation wins.
- Server broadcasts the updated collection after persistence succeeds.

### Create Active Note

**Input**: `workspace_id`, `text`.  
**Output**: updated `WorkspaceNotesCollection` with a new active note.

Rules:
- Trimmed empty text is rejected.
- Original text, including interior and trailing newlines, is preserved.
- New note appears in the active list for the same workspace only.
- Draft is cleared after successful creation.
- Mutation is persisted before ack/broadcast.

### Edit Note

**Input**: `workspace_id`, `note_id`, `text`.  
**Output**: updated active or archived note in the collection.

Rules:
- Trimmed empty text is rejected.
- The note must belong to the target workspace.
- Editing an archived note does not reactivate it.
- `updated_at_ms` changes on success.
- Last server-received edit wins.

### Archive Note

**Input**: `workspace_id`, `note_id`, `reason`.  
**Output**: updated collection with note moved from active to archived.

Rules:
- `reason` is `done` or `removed`.
- The note must be active before archiving.
- Archived note receives `archived_at_ms` and keeps original creation time.
- Archived note disappears from active queries and hover preview immediately
  after broadcast.

### Bulk Edit Archived Notes

**Input**: `workspace_id`, list of `(note_id, text)` updates.  
**Output**: updated archived notes in the collection.

Rules:
- All note IDs must belong to the workspace archive.
- Empty replacements are rejected per note.
- Active notes are not modified by bulk archive editing.
- Save is all-or-nothing for valid input collected in one edit flow.
- Mutation is persisted before ack/broadcast.

## Query Operations

### Collections Snapshot

Returns note collections for requested workspace IDs.

Rules:
- Missing workspaces return empty collections or are omitted consistently by
  the protocol contract.
- Archived notes and drafts are included because clients need modal state.
- Snapshot data is server-authoritative at send time.

### Active Notes for Workspace

Returns active notes for a workspace in display order.

Rules:
- Archived notes are excluded.
- Missing workspace returns an empty list.

### Archived Notes for Workspace

Returns archived notes for a workspace in display order.

Rules:
- Active notes are excluded.
- Missing workspace returns an empty list.

### Hover Summaries

Returns compact summaries for active notes.

Rules:
- Archived notes are excluded.
- Stored text is not mutated by truncation.
- The total active count is returned so UI can indicate overflow.

## Compatibility

Version 1 is the first server-owned note store. It has no migration requirement
from the earlier client-local implementation. Future changes that alter stored
shape must document a migration or compatibility fallback before implementation.
