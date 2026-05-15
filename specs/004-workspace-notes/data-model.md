# Data Model: Workspace Notes

## ServerWorkspaceNotesStore

Authoritative durable root object for all workspace note collections loaded by
one Scribe server process.

**Fields**:
- `version`: schema version for compatibility decisions.
- `owner`: literal `server` marker used to ignore legacy client-local files.
- `updated_at_ms`: last successful persisted mutation time.
- `workspaces`: map from `WorkspaceId` to `WorkspaceNotesCollection`.

**Relationships**:
- Owns zero or more `WorkspaceNotesCollection` records.
- Belongs to the active install flavor's server state root.
- Does not own workspace layout, tabs, panes, PTY sessions, or client modal
  presentation state.

**Validation Rules**:
- `version` must be supported by the server before data is used.
- Unknown future versions produce an empty store and a warning rather than
  blocking terminal startup.
- Workspace keys must parse as valid `WorkspaceId` values.
- Mutations are acknowledged only after the store is durably written.

## WorkspaceNotesCollection

The complete server-owned notes state for one workspace.

**Fields**:
- `workspace_id`: workspace identity.
- `active_notes`: ordered list of active `WorkspaceNoteEntry` records.
- `archived_notes`: ordered list of archived `WorkspaceNoteEntry` records.
- `draft`: optional `WorkspaceNoteDraft`.
- `updated_at_ms`: last mutation time for this workspace.

**Relationships**:
- Belongs to one workspace ID.
- Active and archived lists contain note entries created for that workspace.
- Draft belongs to the same workspace and is not a saved note until committed.
- Sent to clients through snapshots and change broadcasts.

**Validation Rules**:
- A note ID must appear in either active notes or archived notes, not both.
- Active notes must have `status = active`.
- Archived notes must have `status = archived`, `archived_at_ms`, and an archive
  reason.
- Empty collections may be kept to preserve drafts or future workspace returns.
- Last server-received mutation wins when concurrent clients edit the same
  collection.

## WorkspaceNoteEntry

A saved note entry authored by the user.

**Fields**:
- `note_id`: stable note identity.
- `workspace_id`: owning workspace.
- `text`: note body, including newlines.
- `status`: `active` or `archived`.
- `created_at_ms`: creation timestamp.
- `updated_at_ms`: last edit timestamp.
- `archived_at_ms`: timestamp when moved to archive, present only for archived
  notes.
- `archive_reason`: `done` or `removed` when archived.

**Relationships**:
- Belongs to exactly one `WorkspaceNotesCollection`.
- Can be edited while active or archived.
- Archived notes remain recoverable from the archive view.

**Validation Rules**:
- `text` must contain non-whitespace content after trimming.
- `created_at_ms` must be present.
- `updated_at_ms` must be greater than or equal to `created_at_ms`.
- Archived notes must not appear in active-note or hover-preview query results.

## WorkspaceNoteDraft

Server-persisted unsaved note text for one workspace's modal editor.

**Fields**:
- `workspace_id`: owning workspace.
- `text`: draft body, including newlines.
- `updated_at_ms`: last edit timestamp.
- `dirty`: whether the draft differs from an empty state or last saved active
  note creation.

**Relationships**:
- Belongs to one `WorkspaceNotesCollection`.
- Becomes a `WorkspaceNoteEntry` when Ctrl+Enter saves non-empty text.
- Is updated from debounced client draft messages and lifecycle flushes.

**Validation Rules**:
- Whitespace-only drafts may be preserved as drafts but cannot become saved
  notes.
- A dirty draft must be preserved on modal close, workspace switch, shutdown,
  and server restart after the server acknowledges the draft mutation.

## WorkspaceNotesMutation

Typed server command payload for a client-requested note change.

**Variants**:
- `SaveDraft { workspace_id, text }`
- `CreateActiveNote { workspace_id, text }`
- `EditNote { workspace_id, note_id, text }`
- `ArchiveNote { workspace_id, note_id, reason }`
- `BulkEditArchived { workspace_id, updates }`

**Relationships**:
- Applied by `ServerWorkspaceNotesStore`.
- Produces one updated `WorkspaceNotesCollection` on success.
- Produces an error and no broadcast on validation or persistence failure.

**Validation Rules**:
- Every variant must name its target workspace.
- Note IDs must belong to the named workspace.
- Empty saved note text is rejected.
- Bulk archived edits are all-or-nothing.

## ClientWorkspaceNotesCache

Non-durable client-side cache of server note snapshots used by render and event
paths.

**Fields**:
- `collections`: map from `WorkspaceId` to last server-sent
  `WorkspaceNotesCollection`.
- `pending_draft_sync`: optional debounce marker for current modal draft text.
- `last_error`: optional transient error text for failed mutations.

**Relationships**:
- Populated from `WorkspaceNotesSnapshot` and `WorkspaceNotesChanged` server
  messages.
- Read by `WorkspaceNotesModal` and `WorkspaceNotesHoverPreview`.
- Does not write note data to disk.

**Validation Rules**:
- Cached data is replaceable by any newer server snapshot or broadcast.
- Cache misses should render empty states and request/await server data.
- Cache contents are not authoritative after reconnect until refreshed.

## WorkspaceNotesModalState

Transient client UI state for the notes modal.

**Fields**:
- `workspace_id`: workspace being edited.
- `view`: `active` or `archive`.
- `editor_text`: current local editor text.
- `edit_target`: optional note ID currently being edited.
- `archive_edit_mode`: `single` or `all` when archive view is active.
- `scroll_offset`: visible row offset for long note lists.
- `pending_save`: whether a mutation request is in flight.

**Relationships**:
- Reads note collections from `ClientWorkspaceNotesCache`.
- Sends `WorkspaceNotesMutation` messages through client IPC.
- Uses current workspace rectangles for centering and hit testing.

**Validation Rules**:
- Modal state must be scoped to an existing or recently clicked workspace ID.
- Keyboard events are consumed by modal state while open.
- Closing or switching workspaces must force draft sync before losing local
  editor text.

## WorkspaceNotesHoverPreview

Transient preview for active notes under a workspace tab.

**Fields**:
- `workspace_id`: hovered workspace.
- `anchor_rect`: workspace tab or badge rectangle.
- `active_summaries`: visible active note summary lines.
- `total_active_count`: count of active notes for overflow indication.
- `hovered_note_id`: optional visible note row currently under the pointer.

**Relationships**:
- Derived from `ClientWorkspaceNotesCache`.
- Rendered only while the pointer remains over the workspace tab/badge or the
  preview bounds.
- A visible row click sends an `ArchiveNote { reason: done }` mutation.

**Validation Rules**:
- Archived notes are excluded.
- Preview must not request keyboard focus.
- Long entries are truncated visually without mutating stored text.

## State Transitions

```text
Draft(empty) --type text--> Draft(dirty server-debounced)
Draft(dirty) --flush/close/switch/shutdown--> Draft(server persisted)
Draft(dirty) --Ctrl+Enter non-empty--> Active Note
Active Note --edit save--> Active Note(updated)
Active Note --done/remove--> Archived Note
Archived Note --edit save--> Archived Note(updated)
Archived Notes --edit all save--> Archived Notes(updated)
```

Invalid transitions:
- Empty draft to active note.
- Archived note to active note in v1.
- Hover preview to edit mode.
- Cross-workspace movement of a note entry.
- Client cache write to durable note storage.
