# beads-card-detail

## Status

Shipped on 2026-08-15 under epic `scribe-5wh1`.

The as-built architecture lives in the canonical
[client](../lat.md/client.md#guarded-issue-writes),
[protocol](../lat.md/protocol.md#beads-issue-writes),
[server](../lat.md/server.md#beads-issue-writes), and
[test](../lat.md/test.md#real-beads-board-refresh) sections. This document
records product scope, behavior, limits, and the evidence that closed the
feature.

## Problem statement

The original workspace board exposed five shallow queues. Cards carried only
id, title, priority, blockers, and epic metadata. Reading full issue content,
editing, commenting, claiming, closing, or moving work still required `bd`.

The shipped board keeps that compact overview and adds a complete issue panel
plus guarded drag-to-queue writes. The server remains the only process that
invokes `bd`; the client sends typed requests and repaints from authoritative
responses.

## Goals

- A click within the 2px drag boundary opens an issue panel under the source
  lane. Fresh detail supplies text, labels, people, dates, graph links, and
  comments that do not belong in the board snapshot.
- The tracked, owner-approved mock
  `.impeccable/mocks/beads-card-detail.html` defines panel geometry,
  typography, hierarchy, and collapsed-comment behavior. Live theme colors
  replace its fixed sample palette.
- Title, description, acceptance, notes, design, spec id, priority, type,
  labels, status, claim, close, Undo, and comments persist through typed
  server-side `bd` verbs.
- Local drafts never become tracker truth. Applied writes request
  authoritative data; conflicts, failures, and timeouts preserve last-good
  detail and board state.
- Eligible cards drag through GPUI's native window drag layer. Accepted drops
  paint optimistically, then settle from a refreshed snapshot or roll back on
  failure.
- Card drag press, motion, and release never reach PTY mouse reporting. Editor
  keystrokes never reach terminal key encoding.

## Non-goals

- Creating issues from the board or panel.
- Adding, removing, or reparenting dependency links.
- Reordering cards within a lane.
- Dragging cards between regions or workspaces.
- Editing or deleting existing comments.
- Running Dolt sync or producing git side effects.
- Editing due, defer, estimate, external-ref, metadata, assignee, owner, or
  lifecycle dates. A Ready drop may clear defer as part of its status verb.
- Reopening an already closed issue from its panel. Undo exists only for five
  seconds after Scribe itself applies `CloseIssue`.
- Keyboard navigation among panel controls. Esc and editor input routing are
  supported; pointer activation remains the control model.
- Opening an issue without a visible card. There is no open-by-id, search, or
  filter path.
- Bundling Beads or touching Dolt directly.

## Backlog inputs

No external backlog issue fed this work. Implementation was tracked by the
new `scribe-5wh1` epic and its dependent slices.

## Target epic

`scribe-5wh1`, "Beads board card detail panel and drag-to-queue", owns the
shipped work.

## User stories

### Story 1 — Read an issue in full from its card

As a developer working in Scribe, I want to open a board card and read the
whole issue without leaving the terminal.

Acceptance criteria:

- Releasing a card at or inside 2px of the press remains a click. It opens the
  detail panel on hovered and pinned boards.
- The panel anchors below the source lane, stays inside its workspace region,
  targets 560px width, and does not open when less than 400px is available
  after 12px side margins.
- The panel starts 4px below the board. Its maximum height is the lesser of
  70% of region height and remaining region space; overflow scrolls inside.
- Opening lifts and widens the source-card frame into the final geometry over
  120ms. Disabled motion paints the same asserted final layout immediately.
- A card-derived priority, title, and epic remain visible over the loading
  placeholder while the uncached request runs.
- The settled panel matches the approved anatomy: full-width head, identity
  and state rows, dependency spine, sparse body, newest-first comments,
  dependents, and status rail.
- Empty optional sections disappear. Closed detail keeps close facts and
  exposes no write verbs. Blockers render as upstream nodes.
- The newest collapsed comment uses two lines. Older collapsed comments use
  one line. Clicking a comment expands or collapses it in place, and a visible
  hidden count reports comments beyond the newest 50.
- Hovering the identity id reveals its copy glyph without reflow. One click
  drains the full id through the existing take-once clipboard path.
- A dependent click sends one fresh detail request. The source stays visible
  until the reply matches both workspace and target issue, then the panel
  swaps and re-anchors.
- Esc, the close mark, and the backdrop dismiss. A not-found detail reply or a
  later `NotDetected` board closes the panel and leaves a five-second notice.
- Panel state is keyed by workspace. Two regions may keep independent panels;
  one window-scoped editor owns at most one active draft.

### Story 2 — Edit any field and trust it persisted

As a developer, I want panel edits to show tracker-confirmed values, so quick
changes do not require a separate CLI session.

Acceptance criteria:

- Title, description, acceptance, notes, design, spec id, labels, and the
  comment composer use one window-scoped `BeadsEditor`.
- A new edit starts with the stored value selected. Native UTF-16 replacement
  and marked composition update the draft.
- Enter applies a single-line field. Plain Enter remains text in multiline
  fields; a modified Enter applies. Switching fields or blurring applies a
  changed draft. Clicking the same field keeps it. Esc cancels.
- Priority unfolds P0 through P4. Type unfolds the pinned guarded build's
  complete built-in type list. Label input splits on commas or whitespace and
  removes repeats while preserving first appearance.
- The rail's bare words write `open`, `in_progress`, or `closed` with
  `clear_defer: false`. Claim and close use their native verbs.
- Applied `CloseIssue` removes the panel and shows
  `closed <id> · undo`. A click before five seconds sends guarded
  `UndoClose`; the exact deadline sends nothing.
- A nonblank comment queues `AddComment` and leaves the current thread intact.
  Only the matching uncached detail reply adds the persisted row.
- Every panel write copies current detail status and assignee into optional
  guards. `None` means no guard; `Some("")` means the issue must remain
  unassigned.
- One pending or in-flight write is allowed per workspace and issue. A panel
  that navigated elsewhere rejects its stale intent.
- An applied non-close write clears an earlier error and rereads open detail.
  A precondition failure reports "Someone else won" and rereads. Other
  failures keep detail unchanged and show one coral line for five seconds or
  until the next applied result.
- A 15-second client expiry or server timeout marks the outcome unknown,
  requests a board refresh plus detail reread, and blocks another write until
  the first authoritative Ready snapshot reconciles it.
- Reconnect gives every in-flight write the same unknown-outcome treatment.
  Only the first post-reconnect Ready snapshot releases each fence and rereads
  the open issue.
- While the editor owns keyboard focus, terminal focus repair leaves it alone.
  Printable input, Enter, Escape, modifiers, and composition never enter the
  PTY path.

### Story 3 — Move a card between queues by dragging

As a developer triaging work, I want a card drop to use the same guarded write
path as the panel.

Acceptance criteria:

- Backlog, Ready, and In-progress cards register the native drag arm. Blocked
  and Done cards cannot lift.
- Euclidean travel strictly greater than 2px starts a drag. Release at or
  inside the boundary remains the normal panel-opening click.
- Drag state stores workspace, source card, source lane, current window
  pointer, and hovered lane. Every move performs fixed arithmetic and one
  state mutation, with no subprocess, IPC request, or other synchronous I/O.
- Five target lanes share the board's 8px horizontal inset. The right and
  bottom edges are outside, so an out-of-board pointer has no target.
- GPUI paints a source-sized ghost in its native window drag root. It remains
  above other cards and outside lane clipping.
- Hovering Backlog or Blocked reduces that lane's wash to one third. A release
  on either lane, the source lane, or no target queues no write.
- Ready queues typed `SetStatus { status: "open", clear_defer: true }`. In
  progress queues `Claim`. Done queues `CloseIssue` and uses the same
  five-second Undo path as a panel close.
- If the same issue is open, a drop reuses fresh detail status and assignee
  guards. Otherwise Ready and In-progress sources contribute their known
  snapshot status, while assignee and Backlog status remain unguarded because
  compact cards do not carry them. The server applies every supplied guard
  atomically.
- An accepted drop moves the snapshot card immediately and records an
  optimistic overlay. `Applied` tags it with the root generation. Failure
  restores the source lane. The next authoritative snapshot removes the
  overlay.
- If the classifier returns a third lane after an applied write, that lane
  wins and a five-second board notice names the outcome.
- From the armed press through release, the board consumes terminal mouse
  press, motion, and release. A hover-opened board remains open until the
  gesture ends, including while the pointer is outside it.
- Visual evidence measures the ghost within 3px of pointer minus the
  threshold-crossing cursor offset over another card, over a no-drop lane,
  and outside the board. Functional evidence proves persisted claim, close
  and Undo, defer clearing,
  classifier-won behavior, rejected drops, and zero PTY mouse-frame growth.

### Story 4 — See the graph and act on state truthfully

As a developer, I want queue and graph claims to match the server's Beads
classification.

Acceptance criteria:

- The detail response carries the server-selected queue and derivation basis.
  The client does not reclassify detail.
- Classification precedence remains Done, Blocked, In progress, Ready, then
  Backlog.
- The UNBLOCKS row lists dependents returned by
  `bd show --include-dependents`. Blockers render above the issue node.
- The first authoritative snapshot after a write wins over optimistic target
  placement. An issue set open while still blocked therefore returns to
  Blocked and receives the classifier notice.
- All panel colors come from live theme slots, ANSI queue hues, and the board's
  contrast solver. Panel text clears 4.5:1 against its actual ground; marks
  use the board's non-text floor. No mock color is copied literally.

## Constraints

- The visual artifact is the tracked, owner-approved
  `.impeccable/mocks/beads-card-detail.html`, approved 2026-08-14, SHA-256
  `d85a36cf2ec2a687379f86908d4e27dca2708364ba68d0367927bb08645f609e`.
  The mock governs geometry, type, hierarchy, and comment folds. This spec
  governs behavior. The live theme governs color.
- The server owns all `bd` execution. Root-scoped reads and writes resolve an
  absolute executable, supply `-C`, set cwd to the canonical project root,
  require schema-1 JSON, bound output, use direct argv, and kill the process
  group on deadline. The rootless version probe only checks the guarded marker.
- Detail reads use a five-second deadline. Writes use a separate 15-second
  deadline because Dolt commit and export are part of the verb.
- Detail and writes require the local owner of a rooted workspace in a
  `SingleController` window. Remote, shared, displaced, and foreign-window
  requests never reach `bd`.
- `Welcome.beads_detail` and `Welcome.beads_write` default false when absent.
  They are server-to-client fields only. Operationally the server advertises
  writes only when detail is available and the guarded build probe passes.
- Scribe does not bundle Beads. Upstream bd 1.1.0 and unrecognized builds keep
  writes disabled. The accepted marker is
  `scribe-guards-7505e173f265`, built from commit
  `7505e173f2659ba6e1f955b86d81a4f9e21810ca` plus
  `docker/beads-guarded-writes.patch`.
- The guarded-build probe runs once per server process. Each read and write
  resolves the executable again, but replacing `bd` does not renegotiate
  `beads_write` until the next server process.
- The server composes exactly the shipped verbs. It rejects empty,
  dash-prefixed, or NUL-containing ids, priorities above P4, and unsupported
  statuses. It truncates comment bodies to the 64KiB field cap.
- Only direct panel and board gestures create write intents. PTY content cannot
  reach the path, and no generic protocol message lets a client supply `bd`
  argv.
- The panel and board remain region citizens. Panel, board, hover, pin, notice,
  and optimistic state stay keyed by workspace.
- Drag tracking remains O(1) per frame and contains no synchronous I/O.
- Runtime evidence runs only through Docker just recipes with
  `--network none`. The Linux X11/Lavapipe visual run proves the native GPUI
  drag geometry used here; it is not a native macOS Metal run.

## Resolved decisions and Beads contract

All product questions were resolved on 2026-08-15 and are reflected directly
in the stories above.

- Backlog is a source but not a target. Ready clears defer and writes open. In
  progress claims. Done closes. Blocked and Done are not sources.
- Close is reasonless and immediate. Scribe supplies the five-second Undo.
- Type is a picker over the patched build's built-in enum. Defer remains
  display-only except for Ready-drop clearing.
- Detail fetches on open, dependent navigation, conflicts, successful
  non-close writes while that issue remains open, timeouts, and reconnect
  reconciliation. It does not poll.
- Closed detail has no verbs. Unsupported-bd local owners receive a read-only
  panel because detail stays available while write remains false. Shared and
  remote participants receive no detail capability, so they keep the compact
  board rather than opening a viewer panel.
- Beads resolves actor identity from the project environment. Scribe does not
  pass `--actor`.
- Comment detail is newest-first and capped at 50 after parsing Beads' oldest-
  first response. The hidden count preserves the omitted total.

The network-none guarded-build contract proves status and assignee checks run
inside native update, label, comment, claim, close, and reopen transactions.
Exit 13 plus structured guard-mismatch JSON maps to `PreconditionFailed`.
Nonzero exit, timeout, spawn failure, invalid argv, and invalid success JSON
map to `Failed` without advancing generation or replacing last-good board
state.

## Architecture approach

The shipped design has three layers. Typed protocol messages carry data and
intent. The server authorizes and executes Beads commands. The client owns
per-workspace presentation plus one window-scoped editor.

### Read path

`RequestBeadsIssueDetail` runs uncached
`bd show --json --include-comments --include-dependents` plus
`bd ready --limit 0`. The response repeats workspace and issue ids, includes
the server-derived queue, and returns `None` only for a vanished issue.

The client parks requests in `BeadsPanels`, renders through the existing free
function, and keeps `BeadsEditor` as the only GPUI entity added for text input.
There is no separate `BeadsDetailPanel` entity.

### Write path

`BeadsPanels` lowers panel and drag gestures into `PanelWriteIntent`. The
terminal view has one IPC exit for those intents. The server validates the
root and connection, serializes writes per canonical root, composes argv from
the typed verb, and returns the correlated result before starting refresh.

An applied write increments a process-local generation for that canonical
root. `refresh_after_write` discards loads that began before a newer committed
generation, then fans the accepted snapshot to every authorized local
`SingleController` workspace on the same root.

### Drag path

Eligible card elements register GPUI `on_drag`. `BeadsBoards` stores the arm,
active drag, pointer target, and optimistic overlay. GPUI owns the window-layer
ghost. Terminal mouse routing checks the shared drag state before encoding PTY
reports. Drop release reuses the normal guarded write queue.

## Affected components

- `crates/scribe-common/src/protocol.rs` defines detail request/response,
  14 typed write verbs, optional guards, three result states, and the two
  default-false `Welcome` fields. `BEADS_BOARD_PROTOCOL_VERSION` remains 1
  because the board snapshot shape did not change.
- `crates/scribe-server/src/beads_board.rs` owns detail parsing and caps,
  executable probing, typed argv, per-root write serialization and generation,
  deadlines, last-good cache behavior, and authoritative refresh.
- `crates/scribe-server/src/ipc_server.rs` owns rooted owner admission,
  request/result correlation, result-before-refresh ordering, and same-root
  board fan-out.
- `crates/scribe-client/src/beads_panel.rs` owns `BeadsPanels`, panel geometry
  and rendering, loading and notice lifecycle, copy/navigation, pickers,
  comments, status actions, write fences, reconnect convergence, and the
  `BeadsEditor` entity.
- `crates/scribe-client/src/beads_board.rs` owns drag arm and target state,
  native ghost presentation, no-drop wash strength, optimistic card movement,
  rollback, and classifier settlement.
- `crates/scribe-client/src/main.rs` owns capability latching, IPC drains,
  editor key precedence and focus arbitration, panel dismissal, PTY mouse
  gating, and server-message reconciliation.
- `docker/Dockerfile.func`, `docker/Dockerfile.visual`, and `tests/e2e/`
  contain the patched Beads build, isolated project fixtures, visual matrix,
  wire tap, and real-bd receipts.

## Data model

No persistent Scribe storage changed and no migration exists.

- `BeadsIssueDetail` carries id, title, description, acceptance, notes,
  design, optional spec id, status, priority, type, labels, optional parent
  epic title, assignee, owner, timestamps, close facts, defer, due, estimate,
  external ref, blockers, dependents, newest 50 comments, hidden count, queue,
  and queue basis. Text remains bounded to 64KiB per field.
- `BeadsIssueWrite` has `SetTitle`, `SetDescription`, `SetAcceptance`,
  `SetNotes`, `SetDesign`, `SetSpecId`, `SetPriority`, `SetType`, `SetLabels`,
  `SetStatus`, `Claim`, `CloseIssue`, `UndoClose`, and `AddComment`.
- `BeadsIssueWriteGuards` carries optional `if_status` and `if_assignee`.
- `BeadsIssueWriteResult` is `Applied { generation }`,
  `PreconditionFailed`, or `Failed { reason }`.
- Client state consists of the per-workspace open panel, requests,
  navigation target, notices, pending and in-flight writes, deadlines,
  reconnect fences, expanded comments, one parked copy, one window editor,
  and board-owned arm, drag, and optimistic-drop records.
- Server state adds one write lock and process-local generation per canonical
  root plus one process-wide guarded-build probe result.

## API and interface changes

- `ClientMessage::RequestBeadsIssueDetail { workspace_id, issue_id }` asks for
  one fresh issue.
- `ClientMessage::BeadsIssueWrite { workspace_id, issue_id, verb, guards }`
  sends one typed mutation without exposing `bd` argv.
- `ServerMessage::BeadsIssueDetail { workspace_id, issue_id, detail }`
  correlates success or not-found.
- `ServerMessage::BeadsIssueWriteResult { workspace_id, issue_id, result }`
  correlates one mutation outcome.
- `ServerMessage::Welcome` adds `beads_detail` and `beads_write`. Older servers
  decode as incapable. A current local owner with ordinary upstream Beads may
  read detail but sees inert write controls.

## Testing strategy

All runtime suites use their existing Docker just recipe with
`--network none`; none touch the host Scribe process or tracker.

### Unit and protocol evidence

- Named MessagePack round trips cover the detail request, complete response,
  not-found response, every write verb with both guards and with neither,
  every result, and independent default-false `Welcome` fields.
- Server tests cover detail envelope shapes and caps, argv for every verb,
  optional and empty-assignee guards, marker parsing, rc13 mapping, timeout
  process-group cleanup, last-good preservation, generation fencing, and
  same-root authorized fan-out.
- Client tests cover panel anatomy and sparse omission, loading, closed and
  blocked forms, 4.5:1 text contrast, comment clamp and hidden count, 400px
  floor, 70% height, min/max board height, 0.8 and 1.6 text scale, 120ms final
  frame, re-anchoring, two-region independence, copy and navigation, editor
  commit/focus rules, all write surfaces, notice expiry, timeout and reconnect,
  strict drag threshold, target/source matrices, PTY ownership, optimism,
  rollback, classifier outcomes, and exact Undo deadline.
- `just docker-unit-beads-write` runs the focused server and client contract in
  the functional build image.

### Visual evidence

- `just e2e-visual-beads-detail-fixtures` mounts the tracked mock, injects
  loading, closed, blocked, comment-clamped, and hidden-count fixtures, and
  checks the 560px geometry, 12px/4px offsets, anatomy pixels, sparse omission,
  epic and priority ink, two-line/newest and one-line/older comment folds,
  expand/re-collapse, ID hover/copy, dependent navigation, three dismissals,
  and not-found/NotDetected notices.
- Its main evidence includes `beads-detail-inventory.json`,
  `beads-detail-loading-panel.png`, `beads-detail-comment-clamped.png`,
  `beads-detail-comment-expanded.png`, `beads-detail-id-hover.png`,
  `beads-detail-hidden-count.png`, `beads-detail-not-found.png`, and
  `beads-detail-not-detected.png`.
- `just e2e-visual-beads-board` captures
  `beads-board-drag-over-cards.png`, `beads-board-drag-no-drop.png`, and
  `beads-board-drag-outside.png`. Each synchronized waypoint measures the
  source-sized ghost within 3px of pointer offset and keeps terminal row count
  unchanged. It also checks the rejected-lane wash repaint and hover hold-open.

### Functional evidence

- `just e2e-func-beads-write-contract` proves the exact marker, guarded native
  field, label, status, comment, claim, close, and reopen behavior, explicit
  unassigned matching, rc13 mismatch JSON, actor and lease, close facts, and
  reopen cleanup.
- `just e2e-func-beads-issue-write` sends one representative of every write
  family through the real server, requires `board_pushed: true`, and rereads
  with `bd show`. A seeded guard race leaves its comment absent. Forced
  nonzero and timeout results preserve the persisted title and last-good
  board. Evidence is `beads-write-fields.json`,
  `beads-write-final-show.json`, and `beads-write-last-good.json`.
- `just e2e-func-beads-board` proves real detail parsing and ID copy, zero
  editor `KeyInput`, zero premature or cancelled editor writes, nonzero and
  timeout notices with at least 500 changed panel pixels, and timeout
  board/detail rereads. It then proves native drag claim, close and Undo,
  defer clearing, classifier-won notice, same/derived-lane rejection, and zero
  SGR mouse-frame growth.
- Its main receipts are `beads-real-detail-evidence.json`,
  `beads-real-bd-show.json`, `beads-write-gpui-final-show.json`,
  `beads-write-last-good.png`, `beads-write-nonzero-notice.png`,
  `beads-write-timeout-notice.png`, `beads-drag-close-notice.png`,
  `beads-drag-classifier-notice.png`, and `beads-drag-functional.png`.

## Deliberate limits and recovery behavior

- A region narrower than the 400px panel floor keeps the board but opens no
  panel.
- Shared, remote, displaced, and foreign-window clients receive no detail or
  write capability. They keep the compact board.
- Most upstream Beads installs are detail-only because Scribe requires the
  exact patched marker for writes and does not ship that binary.
- Editor arrow, Delete, and Tab keys are consumed rather than providing caret
  navigation. Editing starts selected and supports replacement, Backspace,
  composition, commit, and cancel.
- A timed-out write has an unknown outcome. Scribe never retries the mutation;
  it reads board and detail until authoritative state returns.
- A failed optimistic drop restores the source immediately. A successful drop
  remains provisional until the authoritative snapshot removes its overlay.
- Docker X11/Lavapipe supplies the shipped visual evidence. Native macOS Metal
  validation, when required, remains restricted to the repository's hosted
  macOS workflow.

## Sequencing

The implementation landed in dependency order; every stage below is complete.

1. Probe bd 1.1.0 and the checksum-pinned guarded build. Pin the marker,
   guard semantics, actor/lifecycle behavior, and deadlines.
2. Add typed detail protocol, parser, owner/root gate, complete fixtures, and
   the read-only panel with lifecycle, copy, and dependent navigation.
3. Add typed write protocol, server executor, generation fence, root fan-out,
   editor focus path, all field controls, notices, timeout convergence, and
   reconnect reconciliation.
4. Add strict drag tracking and source restrictions, then native ghost, no-drop
   wash, PTY gate, hover hold-open, optimistic settlement, classifier notice,
   and Undo.
5. Prove read, write, and drag through unit, visual, guarded-build, server IPC,
   and real-bd GPUI suites.
6. Consolidate the canonical read, guarded-write, and drag architecture in
   `lat.md`, then sync this spec to those shipped contracts.

## Canonical documentation

- [Client Beads data source](../lat.md/client.md#beads-board-cli-data-source)
- [Guarded issue writes](../lat.md/client.md#guarded-issue-writes)
- [Board interaction and issue detail](../lat.md/client.md#board-interaction-and-issue-detail)
- [Protocol detail and writes](../lat.md/protocol.md#beads-issue-detail)
- [Server Beads issue writes](../lat.md/server.md#beads-issue-writes)
- [Real Beads Board Refresh](../lat.md/test.md#real-beads-board-refresh)
- [Beads card-detail fixtures](../lat.md/test.md#beads-card-detail-fixtures)
- [Beads card drag tracking](../lat.md/test.md#beads-card-drag-tracking)
