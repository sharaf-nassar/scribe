# Research: Workspace Notes

## Decision: Server Owns Workspace Notes

Store workspace notes and drafts in `scribe-server`, keyed by `WorkspaceId`,
with clients sending typed mutation requests and rendering server snapshots.

**Rationale**: Workspace identity, update handoff, reconnect, and multi-window
coordination are server-owned. Client-local persistence created a data-loss
window when package updates killed or relaunched clients before debounced notes
were flushed.

**Alternatives considered**:
- Keep client-local persistence: rejected because it repeats the current design
  flaw and cannot coordinate multi-window edits.
- Hybrid saved-notes-on-server/drafts-on-client: rejected because unsaved drafts
  are also user content and can be lost during update/restart.
- Store in config: rejected because notes are user content with frequent writes,
  not configuration.

## Decision: Start Fresh Instead of Importing Client-Local Notes

The server-backed implementation creates and owns a new note store and does not
automatically import earlier client-local `workspace_notes.toml` files.

**Rationale**: The user explicitly chose a clean server-backed implementation.
Avoiding import reduces migration complexity and prevents stale client-local
data from shaping the new authoritative store.

**Alternatives considered**:
- One-time automatic import: rejected by clarification.
- Prompt before import: rejected by clarification.
- Backup-only migration: rejected because the server feature should not depend
  on the earlier client file.

## Decision: Write-Through Persistence Before Ack/Broadcast

Persist every accepted server mutation before acknowledging it to the requester
or broadcasting the resulting collection to other clients.

**Rationale**: Notes are small and durability matters more than throughput. This
guarantees that any acked or broadcast note state survives server restart,
update, or handoff fallback.

**Alternatives considered**:
- Debounced server disk writes: rejected because acked state could still be
  lost if the server exits before the debounce fires.
- Persist only during shutdown/handoff: rejected because forced updates and
  crashes could lose recent changes.
- Append-only journal: deferred as unnecessary complexity for v1 note volume.

## Decision: Last Server-Received Mutation Wins

If multiple clients mutate the same note or draft, the server applies mutations
in receive order and broadcasts the resulting collection after each accepted
mutation.

**Rationale**: This is deterministic, simple to reason about, and avoids adding
collaboration conflict UI to a terminal note feature.

**Alternatives considered**:
- Version conflict rejection: rejected because it requires retry/conflict UI.
- Per-client drafts: rejected because drafts are workspace-scoped in the spec.
- Edit locks: rejected because locks can become stale and add modal complexity.

## Decision: Debounced Server Draft Updates With Forced Flushes

Clients send debounced draft updates while the user types and force a final
draft sync on modal close, workspace switch, shutdown, and note save.

**Rationale**: This keeps typing responsive while ensuring unsaved draft text is
server-owned before lifecycle transitions that previously caused data loss.

**Alternatives considered**:
- Save drafts only on modal close: rejected because update/restart can occur
  while the modal is open.
- Keep drafts local until Ctrl+Enter: rejected because unsaved drafts are part
  of the durability requirement.
- Persist every keystroke synchronously: rejected because it risks IPC and disk
  churn without meaningful user benefit over debounce plus forced flush.

## Decision: Typed Workspace Notes IPC

Add typed note data and message variants to `scribe-common::protocol`: clients
request snapshots and send note mutations; the server returns snapshots and
broadcasts changed collections.

**Rationale**: Existing Scribe client/server communication already uses typed
MessagePack frames and explicit enum variants for session, workspace, update,
release, and automation flows. Notes should follow the same transport instead
of inventing side channels.

**Alternatives considered**:
- Read server state files directly from the client: rejected because it creates
  two readers/writers and bypasses server authority.
- Piggyback notes into terminal session metadata: rejected because notes are
  workspace-scoped, not session-scoped.
- Use settings webview messaging: rejected because the notes UI lives in client
  chrome, not settings.

## Decision: Client Keeps Only Presentation State

Keep `WorkspaceNotesModal`, hover preview layout, hit targets, active/archive
view selection, and scroll offsets in the client, but source note data from a
server-backed cache updated by snapshots and broadcasts.

**Rationale**: The modal and preview are GPU-rendered client chrome. Their
layout and focus behavior should remain local, while durable note data belongs
to the server.

**Alternatives considered**:
- Server-driven UI state: rejected because the server should not know about
  pointer hover, modal scroll, or local edit mode.
- Client-owned durable cache: rejected because it recreates the old authority
  split.
