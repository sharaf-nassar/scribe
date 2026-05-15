# Contract: Workspace Notes Protocol

## Purpose

Defines the internal client/server IPC contract for server-backed workspace
notes. Messages use the existing Scribe length-prefixed MessagePack transport
and typed enums in `scribe_common::protocol`.

## Data Types

### WorkspaceNotesCollection

Server-authoritative note state for one workspace.

Fields:
- `workspace_id: WorkspaceId`
- `active_notes: Vec<WorkspaceNoteEntry>`
- `archived_notes: Vec<WorkspaceNoteEntry>`
- `draft: Option<WorkspaceNoteDraft>`
- `updated_at_ms: u64`

### WorkspaceNoteEntry

Saved active or archived note.

Fields:
- `note_id: String`
- `workspace_id: WorkspaceId`
- `text: String`
- `status: WorkspaceNoteStatus`
- `created_at_ms: u64`
- `updated_at_ms: u64`
- `archived_at_ms: Option<u64>`
- `archive_reason: Option<ArchiveReason>`

### WorkspaceNoteDraft

Server-persisted unsaved draft text.

Fields:
- `workspace_id: WorkspaceId`
- `text: String`
- `updated_at_ms: u64`
- `dirty: bool`

### WorkspaceNotesMutation

Client-requested mutation.

Variants:
- `SaveDraft { workspace_id: WorkspaceId, text: String }`
- `CreateActiveNote { workspace_id: WorkspaceId, text: String }`
- `EditNote { workspace_id: WorkspaceId, note_id: String, text: String }`
- `ArchiveNote { workspace_id: WorkspaceId, note_id: String, reason: ArchiveReason }`
- `BulkEditArchived { workspace_id: WorkspaceId, updates: Vec<(String, String)> }`

## Client Messages

### WorkspaceNotesGet

Requests authoritative note collections for one or more workspaces.

Payload:
- `workspace_ids: Vec<WorkspaceId>`

Rules:
- Client sends after `SessionList`, after workspace creation, and after
  reconnect when its note cache may be stale.
- Empty `workspace_ids` may request all known collections if implementation
  chooses to support that; otherwise it returns an empty snapshot.

### WorkspaceNotesMutate

Requests a server-side note mutation.

Payload:
- `mutation: WorkspaceNotesMutation`

Rules:
- Server validates the mutation against its current store.
- Server persists the resulting store before sending any success message.
- Server sends `Error` to the requester on validation or persistence failure.
- Server does not broadcast failed mutations.

## Server Messages

### WorkspaceNotesSnapshot

Response to `WorkspaceNotesGet`.

Payload:
- `collections: Vec<WorkspaceNotesCollection>`

Rules:
- Client replaces matching cached collections with the snapshot contents.
- Snapshot does not imply a mutation occurred.

### WorkspaceNotesChanged

Broadcast after one accepted and persisted mutation.

Payload:
- `collection: WorkspaceNotesCollection`

Rules:
- Sent to the requesting client and all other connected clients.
- Acts as the success acknowledgement for the requester.
- Represents the server-authoritative state after write-through persistence.
- Clients replace their cached collection for `collection.workspace_id`.

## Error Behavior

Errors use existing `ServerMessage::Error`.

Required cases:
- Empty saved note text.
- Note ID not found in the target workspace.
- Attempt to archive an already archived note.
- Bulk archive edit includes any invalid note ID or empty replacement text.
- State path cannot be resolved.
- Atomic persistence write fails.

Rules:
- Error responses do not mutate client caches.
- Clients keep local modal text after failed mutations so the user can retry.
- Server logs persistence failures with enough context to identify workspace
  and operation without dumping full note text.

## Ordering and Concurrency

- The server processes note mutations in receive order per connection and uses
  its existing task scheduling to serialize store mutations.
- The last mutation received and accepted by the server wins.
- Each accepted mutation produces one `WorkspaceNotesChanged` broadcast after
  persistence succeeds.
- Clients that receive out-of-date local state replace it with the newest
  broadcast they receive.

## Attach and Reconnect

- The initial `SessionList` remains focused on session/workspace topology.
- After `SessionList`, the client requests note snapshots for visible or known
  workspaces using `WorkspaceNotesGet`.
- Reconnected clients refresh their cache before rendering modal or preview
  state for a workspace.

## Handoff and Restart

- Write-through persistence is the primary durability mechanism.
- No note mutation is acknowledged or broadcast before disk persistence.
- A new server after hot reload or cold restart loads the server-owned
  `workspace_notes.toml` before serving note snapshots.
- Handoff state does not need to carry dirty note mutations because accepted
  mutations are already persisted.
