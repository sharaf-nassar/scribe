# Implementation Plan: Workspace Notes

**Branch**: `004-workspace-notes` | **Date**: 2026-05-15 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/004-workspace-notes/spec.md`

## Summary

Rebuild workspace notes with the server as the authoritative owner. The client
keeps the GPU modal, hover preview, keyboard focus, and mouse interactions, but
all note and draft state moves behind typed IPC commands handled by
`scribe-server`. The server persists each accepted mutation before acknowledging
or broadcasting it, broadcasts the resulting workspace note collection to every
connected client, ignores earlier client-local note files, and uses
last-server-received mutation wins for concurrent edits.

This corrects the current design flaw where update-driven client relaunches can
lose debounced client-local note state. The new implementation treats notes as
durable server state scoped by `WorkspaceId`, while preserving the existing
terminal UX and avoiding live-server restarts during development.

## Technical Context

**Language/Version**: Rust 2024, workspace rust-version 1.87
**Primary Dependencies**: Existing `serde`/`toml` for persisted note data,
`tokio` and existing IPC framing for server/client messages, `winit` for client
input routing, and `wgpu`/`scribe-renderer`/`cosmic-text` for GPU-rendered modal
and preview chrome
**Storage**: Server-side TOML under the active flavor's state root via
`scribe_common::app::current_state_dir()`, using a new server-owned
`workspace_notes.toml` store. Existing client-local `workspace_notes.toml` files
are not imported automatically.
**Testing**: Planning only in this phase. The user has not requested new test
code, so the implementation plan defines manual quickstart verification and may
propose focused automated tests only if explicitly approved later.
**Target Platform**: Scribe desktop terminal on Linux and macOS
**Project Type**: Desktop terminal emulator with Rust GPU client and
server-owned PTY/session/workspace lifecycle
**Performance Goals**: Modal editor ready for typing within 150 ms of tab
click; hover preview visible within 100 ms for 50 active notes; note mutation
ack/broadcast latency should remain fast enough that modal controls feel
immediate for ordinary note sizes
**Constraints**: Do not restart the live Scribe server during implementation or
verification without explicit user approval; preserve tab drag, close-button,
equalize, tooltip, selection, mouse-reporting, prompt-bar, and terminal keyboard
routing; avoid forwarding modal keystrokes to PTYs; preserve crate boundaries
and typed IPC patterns
**Scale/Scope**: Per-workspace collections for active notes, archived notes, and
one draft per workspace; hover preview capped to compact visible rows while full
content remains available in the modal; concurrent multi-window edits converge
by last server-received mutation wins

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

Initial gate status: PASS.

- **Code Quality**: PASS. Server ownership follows existing server
  responsibility for workspace identity and update handoff. Protocol additions
  live in `scribe-common`, server persistence lives in a focused server module,
  and client rendering remains in dedicated UI modules.
- **Testing Strategy**: PASS. Every user story has an independent manual
  verification path. New automated test code is not requested, so this plan
  documents manual checks and identifies automated coverage as optional pending
  later approval.
- **User Experience Consistency**: PASS. The client keeps existing workspace
  tab entry points, modal focus behavior, compact terminal chrome language, and
  preview interactions.
- **Performance**: PASS. The plan preserves in-memory snapshots in the client
  for render-time preview speed while pushing durable mutations through typed
  server commands.
- **Operational Safety**: PASS. No live server restart is required. Mutations
  are write-through on the server, which directly addresses update/restart data
  loss. Implementation must update `lat.md` and run `lat check` when behavior
  changes.
- **Protocol/Persistence Compatibility**: PASS. The spec explicitly chooses a
  fresh server-backed store and no automatic import of client-local note files.

## Project Structure

### Documentation (this feature)

```text
specs/004-workspace-notes/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   ├── workspace-notes-protocol.md
│   ├── workspace-notes-state.md
│   └── workspace-notes-ui.md
└── tasks.md
```

### Source Code (repository root)

```text
crates/scribe-common/src/
└── protocol.rs                 # note data types and IPC message variants

crates/scribe-server/src/
├── workspace_notes.rs          # authoritative note store and write-through persistence
├── ipc_server.rs               # note command dispatch and update broadcasts
└── main.rs/session startup     # load note store with other server state

crates/scribe-client/src/
├── ipc_client.rs               # note command senders and note update UI events
├── main.rs                     # modal state integration, draft debounce, render inputs
├── workspace_notes_modal.rs    # modal UI state and hit tests
├── workspace_notes_preview.rs  # hover preview layout/rendering
└── workspace_notes.rs          # remove client-owned persistence; keep client cache helpers if useful

lat.md/
├── protocol.md                 # document note protocol additions
├── server.md                   # document server-owned workspace notes store
└── client.md                   # document client rendering and IPC-backed behavior
```

**Structure Decision**: Move note ownership to `scribe-server` because
workspace IDs and restart/update lifecycles are server-owned. Keep only
presentation state in `scribe-client`: open modal, selected view, edit mode,
scroll offsets, hover targets, and pending draft debounce timers.

## Phase 0 Research Summary

Research decisions are recorded in [research.md](./research.md).

Resolved decisions:
- Server is authoritative for workspace notes and drafts.
- Existing client-local note files are ignored rather than imported.
- Server uses write-through persistence before ack/broadcast.
- Clients send debounced draft updates plus forced lifecycle flushes.
- Concurrent mutations use last server-received mutation wins.
- Note state changes travel over typed protocol messages and broadcasts.
- Client modal and hover preview remain GPU-rendered local chrome fed by a
  server-backed client cache.

## Phase 1 Design Summary

Design artifacts are recorded in [data-model.md](./data-model.md),
[contracts/workspace-notes-protocol.md](./contracts/workspace-notes-protocol.md),
[contracts/workspace-notes-state.md](./contracts/workspace-notes-state.md),
[contracts/workspace-notes-ui.md](./contracts/workspace-notes-ui.md), and
[quickstart.md](./quickstart.md).

Key design points:
- `WorkspaceNotesStore` lives in the server and exposes workspace-scoped
  mutation/query methods.
- `WorkspaceNotesCollection` contains active notes, archived notes, and the
  current server-persisted draft for one workspace.
- Client note caches are derived from server snapshots and
  `WorkspaceNotesChanged` broadcasts; they are not durable stores.
- Mutations are accepted only after successful write-through persistence.
  Persistence failure returns an error and does not broadcast stale state.
- Multi-window clients converge through server broadcasts after every accepted
  mutation.
- Draft typing is debounced client-side, then sent to the server, with explicit
  flush points on modal close, workspace switch, shutdown, and save.

## Constitution Check

*GATE: Re-check after Phase 1 design.*

Post-design gate status: PASS.

- **Code Quality**: PASS. The design separates protocol types, server
  persistence, server dispatch, client IPC, and client rendering. It removes the
  client-owned durable store instead of keeping two authorities.
- **Testing Strategy**: PASS. Quickstart covers note capture, active/archive
  management, hover preview, multi-window convergence, restart persistence, and
  ignored legacy client files. No new test code is planned because the user has
  not requested tests.
- **User Experience Consistency**: PASS. User-facing behavior remains the
  current modal/preview workflow, with better durability and multi-window sync.
- **Performance**: PASS. Render paths use client-side cached snapshots, while
  typed server mutations are limited to user note operations and debounced draft
  updates.
- **Operational Safety**: PASS. The plan avoids live server restarts during
  development. Write-through persistence prevents acked note changes from being
  lost during updates/restarts.
- **Protocol/Persistence Compatibility**: PASS. The new server store starts
  fresh. Old client-local files are ignored and left untouched.

## Complexity Tracking

No constitution violations or complexity exceptions are required.
