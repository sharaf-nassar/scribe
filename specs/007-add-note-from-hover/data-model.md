# Phase 1 Data Model: Add Note From Hover Preview

**Date**: 2026-05-20
**Spec**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md) | **Research**: [research.md](./research.md)

## Summary

This feature introduces **no new persistent entities** (server file, protocol variant, config field). It introduces **one transient client UI state**: `AddingNoteState`, held per workspace on the `App` struct in `scribe-client`.

All durable data flow (saved drafts, active notes, archived notes) goes through the existing `WorkspaceNotesMutation` / `WorkspaceNotesSnapshot` / `WorkspaceNotesChanged` protocol unchanged.

## New transient state

### `AddingNoteState` (per workspace, lives on `App`)

Held in: `App::adding_note_states: BTreeMap<WorkspaceId, AddingNoteState>` (new field, parallel to the existing `workspace_notes_save_pending: Option<Instant>`).

Fields:

| Field | Type | Purpose |
|---|---|---|
| `draft_text` | `String` | Live editor text. Initialized from the workspace's saved draft (FR-002). |
| `draft_dirty` | `bool` | True if local edits have not yet been written through `SaveDraft`. Mirrors the modal's `draft_dirty` flag and gates the snapshot-pristine policy (FR-015 / FR-020). |
| `caret` | `CaretPosition` | Caret index into `draft_text` (line + column or absolute char index — implementation choice). Restored on pointer-return per FR-003. |
| `scroll_offset_rows` | `usize` | Internal scroll offset for when the editor exceeds the 3/4-pane cap (FR-022). Zero by default. Mouse-wheel scroll updates it without moving the caret; typing snaps it to keep the caret visible. |
| `last_server_error` | `Option<String>` | The most recent server rejection message for `CreateActiveNote`. Set when a `WorkspaceNotesChanged` reply carries an error; cleared on the next successful commit or on cancel. (FR-012) |
| `committed_pending` | `bool` | True after the user pressed Enter and the `CreateActiveNote` mutation was sent but the server has not yet broadcast the resulting collection. Used to coalesce duplicate commits (FR-016). |

Lifecycle:

- **Created** lazily by clicking the "+" affordance on a workspace whose preview is in read-only state.
- **Initialized** from the workspace's `SaveDraft` value via the existing `WorkspaceNotesStore` snapshot.
- **Updated** by keystrokes (`draft_text`/`caret`/`scroll_offset_rows`/`draft_dirty`), debounced writes (`SaveDraft` clears `draft_dirty` once acked), and server replies (`last_server_error` set/cleared).
- **Destroyed** on:
  - Enter (FR-006) — after server broadcast of `CreateActiveNote` resolves; map entry removed.
  - Escape (FR-008) — immediately; map entry removed; no `SaveDraft` write.
  - Higher-priority-overlay handoff (FR-010) — `SaveDraft` is flushed if `draft_dirty`, then map entry removed.
  - Window close / app shutdown (FR-014) — same flush-then-remove path.
  - Workspace removal (closing the workspace tab) — map entry removed; orphan defensive cleanup.

Invariant: `draft_text` is always equal to (or a strict superset of, in the case of in-flight typing) the workspace's saved draft via the server. The shared-buffer guarantee in FR-020 is preserved by the "init from saved draft + flush via SaveDraft" pair.

## Existing entities (referenced, not modified)

For completeness — these are the entities this feature reads and writes via the existing protocol:

| Entity | Source | Read / Write |
|---|---|---|
| `WorkspaceNotesStore` (client) | `crates/scribe-client/src/workspace_notes.rs` | Read for the saved-draft text used to initialize `AddingNoteState`. |
| `WorkspaceNotesStore` (server) | `crates/scribe-server/src/workspace_notes.rs` | Untouched. |
| `WorkspaceNotesMutation::SaveDraft` | `crates/scribe-common/src/protocol.rs:122` | Written by the inline editor's debounce pipeline (existing path). |
| `WorkspaceNotesMutation::CreateActiveNote` | `crates/scribe-common/src/protocol.rs:122` | Written by the inline editor on Enter. |
| `WorkspaceNotesMutation::ArchiveNote { reason: Done }` | `crates/scribe-common/src/protocol.rs:122` | Continues to be written by clicks on existing read-only note rows in the preview (FR-011). |
| `WorkspaceNoteSummary` | `crates/scribe-client/src/workspace_notes.rs` | Read by the preview to render the read-only rows (existing path). |
| `WorkspaceNotesModal::draft_text` (client) | `crates/scribe-client/src/workspace_notes_modal.rs:68` | The modal's view of the shared draft buffer; logically the same buffer as `AddingNoteState::draft_text` for the same workspace (FR-020). |

## State transitions

```
                            ┌──────────────────────────────────┐
                            │   Preview in read-only mode      │ ← starting state for any workspace
                            │   (no AddingNoteState entry)     │   without an entry in App::adding_note_states
                            └────────────────┬─────────────────┘
                                             │
                                  click "+" affordance
                                             │
                                             ▼
                            ┌──────────────────────────────────┐
                            │   AddingNoteState created        │
                            │   draft_text ← saved SaveDraft   │
                            │   draft_dirty = false            │
                            │   caret = end                    │
                            │   scroll_offset_rows = 0         │
                            │   last_server_error = None       │
                            │   committed_pending = false      │
                            └────────────────┬─────────────────┘
                                             │
                  ┌──────────────────────────┼──────────────────────────┐
                  │                          │                          │
              user types                press Enter                 press Esc                 higher-priority overlay
              (modify draft)         (with non-empty trimmed)       (cancel)                  opens
                  │                          │                          │                          │
                  ▼                          ▼                          ▼                          ▼
       draft_dirty=true              committed_pending=true       delete entry              if draft_dirty:
       debounce SaveDraft            send CreateActiveNote        no SaveDraft               flush SaveDraft
       (clears dirty on ack)         on broadcast: remove         on send                   delete entry
                                     entry                                                    (no overlay-specific
                                                                                                semantics; entry gone)
```

Notes:

- The "user types" loop runs while `committed_pending == false` (FR-016 coalesces duplicate commits).
- A late `WorkspaceNotesChanged` snapshot during typing does NOT overwrite `draft_text` while `draft_dirty == true` (FR-015 + the existing modal pristine-draft rule).
- A server error in response to `CreateActiveNote` sets `last_server_error` and resets `committed_pending = false`, but leaves `draft_text` intact so the user can retry (FR-012).

## Concurrency model

- Multiple `AddingNoteState` entries can exist simultaneously (FR-021) — one per workspace.
- Within a single workspace, only one editor is open at a time (the "+" affordance disables itself while the workspace's entry exists, per FR-002).
- The `BTreeMap` uses `WorkspaceId` as key — guaranteed unique by the existing workspace model.

## What this data model is *not*

- **Not a new server-side data structure.** The server is unchanged. `SaveDraft` text continues to live in the existing per-workspace store on the server.
- **Not a new protocol field.** All wire messages reuse existing variants.
- **Not a new persistent file.** No client-side persistence is added; `AddingNoteState` is in-memory and discarded on app exit (after the flush in FR-014).
- **Not a copy of the saved draft.** Conceptually, the modal and the inline editor are two views onto the *same logical buffer*. The `AddingNoteState::draft_text` field is the client's working copy of that buffer while the inline editor is open; the modal's `draft_text` field plays the same role when the modal is open. Both flush through `SaveDraft`.
