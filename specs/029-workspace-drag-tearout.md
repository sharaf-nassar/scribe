# workspace-drag-tearout

## Problem Statement

Scribe windows are split into workspace regions (a binary split tree of
`WorkspaceSlot`s), each region carrying a tab strip and — when the server
names the workspace — a colored badge pill. Today the arrangement of those
regions can only be changed indirectly: splits create new regions, dividers
resize them, and closing collapses them. There is no way to *rearrange*
existing workspaces within a window, and no way to move a workspace out of a
window at all.

Users who organize per-project workspaces (the badge pills) expect the same
direct manipulation they get from browser tabs and editor groups: grab the
workspace by its pill, drag it where it should live — another position in
this window, or out of the window entirely to become its own window. The
interaction must feel seamless and smooth: continuous visual feedback,
obvious drop targets, no jank, and a safe cancel.

## Goals

- Drag any workspace's pill to move that workspace region to a different
  position in the same window, with live drop-target preview before release
  (5-zone model: edge bands split, center swaps).
- Drag a workspace pill to/past the window edge to detach the workspace —
  all of its tabs/sessions — into a new window.
- Every region has a pill: server-named workspaces keep their named accent
  pill; unnamed workspaces in multi-region windows render a neutral
  fallback pill so the drag handle always exists.
- Continuous feedback at every stage (NN/G drag-drop guidance): grab
  affordance on the pill, a drag representation that tracks the cursor
  inside the window, drop-zone highlight that fades in/out smoothly
  (150 ms cap, reduced-motion honored), and an animated settle on drop.
- Escape (or release over a non-target) cancels cleanly with zero protocol
  frames and zero focus/selection change.
- Keyboard/palette equivalents in v1: "Move workspace to new window" plus
  directional region moves, producing the same protocol operations as the
  drag paths (constitution P2/P3).
- Sessions survive the move untouched — server-owned sessions never
  restart; scroll state and running programs are preserved across client
  crash and server handoff/upgrade (a true server crash loses PTYs by
  design and is out of this guarantee).
- Server window trees and session→window ownership move in one atomic,
  acknowledged transaction (no orphaned or double-owned sessions at any
  observable point).

## Non-Goals

- Dragging a workspace onto *another existing* Scribe window to merge it
  there — by pointer or by palette. Wayland's implicit-grab model prevents
  the origin window from learning the cursor entered a sibling surface, and
  GPUI has no cross-window drag (Zed itself has none — zed#6722/#6078
  open). Recorded for a phase-2 epic.
- Chrome-style live "window follows the cursor" tear-out. Impossible on
  Wayland (no client window positioning, no global cursor); Chromium and
  Firefox both ship fallback behavior there. The fallback is specced as the
  universal behavior.
- Re-attaching a torn-out workspace by dragging it back into another window
  (phase-2, same epic as cross-window merge).
- Share migration: shares stay keyed to the source `WindowId`. Tearing a
  workspace out of a shared window is allowed; its sessions simply leave
  that share and its viewers lose them (clean detach, no partial frames).
  Migrating participants to the new window is phase-2.
- Dragging individual session *tabs* between workspaces (tab-level drag UX
  is a separate feature; `MoveSession` already exists for commands).
- Reordering *tabs within* a workspace (already shipped).
- Merging two workspaces' identities/tab sets into one workspace.
- Transferring window-local furniture (pinned boards, CI strips, window
  geometry records) with the workspace — the new window starts with default
  furniture.
- Changes to the settings window (PRODUCT.md/DESIGN.md scope untouched).
- Upstream GPUI changes or re-pinning the GPUI rev (e.g. implementing
  `xdg_toplevel_drag`).

## Backlog Inputs

None. No `epic`/`source_backlog` variables were supplied, and a scan of the
open backlog found no open or deferred P4 issues touching workspace drag,
tear-out, badges, or multi-window arrangement (all matches are closed
issues).

## Target Epic

None exists. This run will create a new feature epic for
workspace-drag-tearout.

## Source Authority

- `/tmp/pi-clipboard-f8db13da-396d-427f-8d55-4189d8d836fc.png` (user
  screenshot, transient path): shows the "scribe" workspace badge pill in
  the titlebar tab strip. Authority: reference-only — it identifies *which
  surface* is the drag handle (the badge pill), not a normative visual
  design. No sections are normative; visual design of drag feedback follows
  existing terminal-chrome conventions in `titlebar.rs`/`tab_bar.rs`.
- No HTML mocks, prototypes, or design files are referenced. Terminal
  chrome is explicitly outside PRODUCT.md/DESIGN.md authority (repo guide).

## Prior Art (research summary, informative)

- **Chrome/Chromium**: the benchmark. Live tab contents in the drag, tear-off
  past a band threshold, re-attach by dragging back. Requires window
  positioning; on Wayland Chromium ships Igalia's fallback — detached window
  appears where the compositor puts it and does not follow the cursor.
- **Windows Terminal** (microsoft/terminal#14935): tear-out creates a new
  window inheriting the source window's dimensions, placed at the drop point
  (Win32 can position). Drop back onto another tab strip re-attaches.
- **VS Code**: drag tab beyond the window → floating window; context menu
  "Move into New Window" as the discoverable, accessible alternative;
  between-tab drop indicator for in-strip placement (vscode#203681).
- **Zed (GPUI upstream)**: has in-window pane drops with 5-zone hit testing
  (center = move into, edges = split on that side) but *no* tear-out to a new
  window at all — confirms GPUI gives us no cross-window drag for free.
- **NN/G / Atlassian drag guidance**: signal grabbability (cursor + handle),
  keep feedback continuous through all stages, animate sliding rather than
  popping, make drop zones explicit, always provide a non-drag alternative.

Synthesis: the best achievable UX on our platform set is **in-window spatial
drop targeting (Zed-style zones) + drag-to-edge-to-detach (VS Code-style
outcome)**, with palette commands as the accessible path, and *without*
promising cursor-following windows the Linux stack cannot deliver.

## User Stories

### US1 — Rearrange workspaces within a window

As a user with several workspace regions in one window, I want to drag a
workspace's pill and drop it onto another region's area, so that I can
rearrange my layout directly instead of closing and re-splitting.

Acceptance criteria:
- Every region's pill is a drag source: named pills as today, neutral
  fallback pills for unnamed workspaces in multi-region windows. A plain
  click still focuses the workspace's region exactly as today; the nested
  Beads icon keeps its own click/hover/keyboard behavior and is not part of
  the drag grab area.
- A press that travels past GPUI's native drag threshold (~2 px) arms the
  workspace drag; the pill press stops propagation so the titlebar's
  compositor window-move can never arm from a pill. The workspace drag uses
  its own drag marker type, distinct from `TabDrag`, so tab-reorder routing
  is untouched.
- While dragging, a pill ghost follows the cursor inside the window
  (deferred overlay in the origin window; GPUI cannot paint past its
  surface).
- Hovering over another workspace region shows the 5-zone drop preview:
  the four edge bands (each one-third of the region's width/height) insert
  the dragged workspace on that side by re-splitting the hovered region;
  the remaining center zone swaps the two workspaces' positions. Zone
  transitions carry 4 px hysteresis so boundary hover does not flicker.
  Hovering the source's own region shows no actionable zone (release
  there = cancel).
- Structural moves (edge insertions and the removal they imply) re-equalize
  all region ratios, matching existing `split_workspace`/`remove_workspace`
  behavior; a center swap is a pure leaf exchange preserving tree shape and
  every ratio.
- Releasing over a zone applies the move at release time: the window split
  tree updates, the layout re-renders, and the tree is reported to the
  server. The settle animation is purely visual; with animations disabled
  the observable state is identical.
- Releasing anywhere non-actionable (own region, dividers, chrome outside
  any zone, in-window without a zone) or pressing Escape mid-drag cancels
  with zero protocol frames, zero layout change, and zero focus/selection
  change. Escape during a drag is claimed by the chrome and never reaches
  the PTY.
- All sessions in the moved workspace keep running; the moved workspace
  stays focused with its previously active tab (no select-first-tab side
  effect).
- The updated tree survives client restart/reconnect (`ReportWorkspaceTree`
  persistence as today).

### US2 — Tear a workspace out into its own window

As a user, I want to drag a workspace's pill to the window edge and
release, so that the workspace becomes its own window (e.g. to move it to
another monitor).

Acceptance criteria:
- Arming rule (hybrid, one gesture everywhere): the drag arms tear-out when
  the cursor is within the window-edge band or beyond the window bounds
  (out-of-bounds coordinates are used where the backend delivers them —
  X11/macOS; the inner edge band covers Wayland, where GPUI stops
  delivering motion after surface leave). Exact band width is fixed by the
  platform spike; arming/disarming carries hysteresis so edge hover does
  not flicker.
- Arming is visually explicit: in-window drop previews clear and the ghost
  switches to a "will detach" treatment. Dragging back inside disarms and
  resumes in-window drop targeting — one continuous gesture.
- Releasing while armed opens a new Scribe window containing exactly that
  workspace with all its tabs, active tab, and pane layouts preserved — no
  initial login shell (claimed-window bootstrap path, not
  `initial_session: true`).
- The transfer is one atomic, typed, acknowledged server transaction
  (client-minted transfer id, idempotent via a bounded server-side result
  ledger): it validates ownership, moves every session's window mapping,
  updates both windows' trees (server-derived from shared tree operations
  — the request carries ids only), re-binds the env-envelope/launch-id
  linkage, and re-broadcasts in-flight agent activity leases to the
  destination window. At no observable point is a session unmapped or
  double-owned. Failure contract: any refusal, disconnect, or error
  *before* the server commit leaves the source window byte-identical;
  *after* the ACK the move is committed — if the client then fails to open
  the target window, the sessions are safe under the new `WindowId` and
  are recovered through the existing restored-window offer (recovery UX,
  not rollback).
- `WorkspaceId` and every `SessionId` stay stable; the new window gets a
  fresh `WindowId`; agent `world`/`siblings` snapshots reflect the
  destination atomically at commit.
- The source window's tree collapses the vacated region (sessions keep
  running); source focus follows the existing `remove_workspace`
  first-in-tree rule. Dragging the *only* workspace's pill to the edge is a
  no-op with feedback (ghost snaps back with a brief hint; nothing to
  detach).
- New-window geometry: inherits the source window's size; placed at the
  release cursor (top-left) on X11/macOS, compositor-placed on Wayland.
  UX copy never promises cursor-anchored placement on Wayland.
- If the source window is shared, the transfer proceeds: the moved
  sessions cleanly detach from that share's viewers (no partial frames,
  no dangling sinks); the share itself stays with the source window.
- Peers without the transfer capability (old server or old client) get a
  typed refusal and disabled drag-to-detach UX, never a partial move.

### US3 — Smoothness and feel

As a user, I want the whole gesture to feel native and fluid, so that
rearranging never feels risky or janky.

Acceptance criteria:
- Ghost and drop-zone preview updates render within one 60 Hz frame
  (16.7 ms input-to-paint, p95 on the dev-hardware profile), measured by an
  instrumented drag probe named in the plan.
- Zone highlights fade in/out and the drop settle/snap-back animate under
  the existing `AnimationSettings` policy (150 ms `MAX_TRANSITION` cap,
  `appearance.animations` + `SCRIBE_DISABLE_ANIMATIONS` reduced-motion path
  yields zero-duration with identical end state).
- No PTY input leakage: the entire gesture (down, moves, up, Escape) is
  claimed by the chrome and never reaches terminal mouse reporting
  (learnings: "a surface that handles a gesture must claim it").
- Drag state is fully cleaned up on cancel, window blur, or
  workspace/session disappearance mid-drag (no stuck ghosts, next drag
  works immediately); these interrupts send zero protocol frames.

### US4 — Non-drag alternatives

As a keyboard-focused user, I want palette/command equivalents, so that the
same outcomes are reachable without a pointer.

Acceptance criteria:
- Command palette gains: "Move workspace to new window" and directional
  region moves ("Move workspace left/right/up/down") mirroring the drop
  zones' structural semantics; a directional move with no neighbor on that
  side is a no-op with feedback.
- Actions operate on the focused workspace and produce the same protocol
  operations as the equivalent drag (identical trees and ownership;
  tear-out necessarily mints a fresh `WindowId`).

## Constraints

- **C1 — GPUI pinned rev, per-window drag.** All drag machinery lives in
  Scribe using GPUI's per-window drag (`on_drag`/`on_drag_move` with a
  dedicated marker type). No upstream GPUI patches. GPUI paints drag ghosts
  only inside the origin window; the Wayland backend stops delivering
  motion after surface leave — the hybrid arming rule (US2) exists because
  of this. A platform spike (P0 prerequisite) verifies drag-move delivery
  at/beyond edges, re-entry, release, Escape, and blur on all three
  backends before gesture code lands.
- **C2 — Platforms: Linux Wayland, Linux X11, macOS.** Feature must work on
  all three; behavior may degrade only in window *placement* (Wayland) and
  out-of-bounds event fidelity (covered by the edge band). E2E visual
  tests run under Xvfb (X11); Wayland/macOS evidence comes from the spike
  plus scripted manual verification named in the plan.
- **C3 — Wayland fallback placement.** No global cursor, no client-side
  window positioning, no self-activation. The new window's position (and
  whether it takes focus) is compositor-controlled; UX copy must not
  promise otherwise.
- **C4 — Atomic server transfer.** `workspace_manager.rs` keys trees by
  `WindowId` and maps sessions to windows; today `filter_attachable_sessions`
  rejects cross-window attach and no reassignment path exists. The move
  ships as a new capability-gated protocol operation (typed request,
  validation, single transaction, typed ACK/refusal); mixed-version peers
  degrade to disabled UX. Handoff/upgrade snapshots taken mid-transfer see
  either the old or the new state, never a partial one.
- **C5 — Existing chrome geometry.** Pills render in the titlebar (top
  region) and in lower-region bars (`RegionChrome`); both are drag sources.
  Tab-level drag-reorder, pill click-to-focus, window move from empty
  chrome, and the Beads icon's own gesture surface must all keep their
  current behavior (regression criteria, not just intentions).
- **C6 — Constitution.** P1 (typed protocol change, crate boundaries), P2
  (session continuity, muscle-memory-safe chrome, consistent ratios/focus
  rules), P3 (independent user-reachable verification per story), P4
  (numeric frame budget above, named probe), P5 (Escape/gesture claiming),
  P7 (lat.md sync, capability compatibility notes, no live-server
  disruption during dev).
- **C7 — Repo verification machinery.** `just ready`, clippy `-D warnings`,
  E2E func/visual suites. New chrome interactions get e2e coverage with
  measured offsets (per learnings store), extending the existing
  tab-drag-reorder, workspace-ipc, multi-window-restore,
  server-upgrade-reattach, and agent-world suites; wire/tree oracles are
  preferred over screenshots for transfer semantics.

## Resolved Questions

All nine original open questions were closed by the spec review's technical
triage plus the clarify gate (see `## Clarifications`):

- OQ1 (out-of-bounds events) → hybrid edge-band arming; platform spike is a
  P0 prerequisite bead (C1).
- OQ2 (sole workspace) → no-op with feedback (US2).
- OQ3 (drop-zone model) → 5-zone: edges split, center swaps (US1).
- OQ4 (cross-window merge) → non-goal in v1, pointer and palette; phase-2
  epic.
- OQ5 (palette set) → move-to-new-window + directional moves (US4).
- OQ6 (tear-out geometry) → inherit source window size; cursor placement on
  X11/macOS, compositor on Wayland (US2).
- OQ7 (drag representation) → pill-only ghost inside origin window (US1).
- OQ8 (protocol surface) → first-class atomic capability-gated transfer
  operation (C4).
- OQ9 (reduced motion) → premise was wrong; `appearance.animations` +
  `SCRIBE_DISABLE_ANIMATIONS` + 150 ms cap already exist and are reused
  (US3).

## Spec Review

Six-dimension parallel review (requirements, gaps, ambiguity, feasibility,
scope, stakeholders). All six returned merge verdict BLOCK; findings below
are triaged into product questions (human gate), self-resolved technical
decisions, and non-blocking observations.

### Critical Questions (answer before planning)

1. **In-window drop model** — US1 asserts 5-zone spatial targeting (edges
   split, center swaps) while OQ3 leaves it open; an AC cannot cite an open
   question. Zed-style 5-zone is the honest model for the split tree;
   center-swap-only is the cheaper MVP; linear badge-strip reorder discards
   spatial meaning. Flagged by: requirements, ambiguity, feasibility, scope.
2. **Sole workspace in a window** — drag-out of the only workspace:
   no-op with feedback (cheapest, `WindowLayout` already refuses removing
   its last leaf), compositor window-move (needs its own spike; Wayland
   post-release move impossible), or move+close-source (empty-window
   lifecycle work). Flagged by: requirements, ambiguity, feasibility,
   scope, stakeholders.
3. **Unnamed workspaces have no drag handle** — badge pills render only for
   server-named workspaces (`tab_bar.rs` returns `None` otherwise), so
   US1/US2 are unreachable for unnamed workspaces as specced. Named-only v1
   with palette fallback, or a neutral always-present pill in multi-region
   windows? Flagged by: requirements, ambiguity, scope, stakeholders.
4. **Tear-out arming gesture** — GPUI's Wayland backend clears pointer focus
   on surface leave and ignores subsequent motion, so "release outside the
   window" likely cannot arm on Wayland. Uniform in-window edge-band arming
   everywhere (consistent muscle memory), or hybrid (outside-bounds where
   delivered, edge-band otherwise)? Subject to the OQ1 spike either way.
   Flagged by: ambiguity, feasibility, gaps, stakeholders.
5. **Shared/remote windows** — window shares are keyed by `WindowId`;
   tearing a workspace out from under viewers has undefined semantics, and
   `requires_window_control` today doesn't even gate workspace mutations.
   Restrict v1 tear-out/rearrange to local unshared windows with a typed
   refusal, or define participant migration? Flagged by: gaps, feasibility,
   stakeholders.
6. **Palette command scope** — minimum is "Move workspace to new window";
   directional region-move commands would give keyboard parity with every
   drop zone (P2/P3) but depend on Q1's answer. Include in v1 or defer?
   Flagged by: requirements, scope, stakeholders.
7. **Animation polish at launch** — the request demands "extremely smooth";
   scope review argues animated settle/zone-fade is phase-2 polish. Keep
   animated feedback in v1 (reusing `AnimationSettings`, 150 ms cap,
   reduced-motion honored) or ship static highlight first? Flagged by:
   scope, requirements (P4 budget), stakeholders (reduced motion).

### Technical Decisions (self-resolved — veto at the gate to override)

- **Protocol: one atomic, typed, acknowledged transfer operation** replaces
  the client-choreographed `ReportWorkspaceTree` + reassignment idea (OQ8
  closed): server validates ownership, moves every session's window
  mapping, updates both window trees, and ACKs before the target window
  attaches; idempotent via client-minted transfer id; gated by a `Welcome`
  capability flag so old peers get a typed refusal / disabled UX. All six
  reviews converged here; `filter_attachable_sessions` actively rejects
  cross-window attach today, so no existing path suffices.
- **Identity: `WorkspaceId` and `SessionId`s stay stable; tear-out mints a
  new `WindowId`** — agent `world`/`siblings` snapshots must flip
  atomically at commit; in-flight activity leases are re-broadcast to the
  destination window post-commit.
- **Environment envelope**: the transfer transaction re-binds the window
  env envelope/launch-id linkage so cold restore in the target window
  keeps working (details in plan; `SessionList.launch_id` gating noted).
- **Target-window bootstrap** reuses the claimed-window/no-initial-session
  path (as `open_restored_window` does), never `initial_session: true` —
  "exactly that workspace", no extra login shell.
- **Crash wording**: continuity guarantee scoped to client crash and server
  handoff/upgrade; a true server crash loses PTYs by design (existing E2E
  asserts this). Spec text corrected accordingly.
- **Drag start**: pill press keeps `stop_propagation` (window-move can
  never arm from a pill), new drag marker type distinct from `TabDrag`,
  GPUI's native ~2 px drag threshold arms the drag (same as tab drag).
- **Drag representation (OQ7 closed)**: pill-only ghost painted as a
  deferred overlay inside the origin window — GPUI cannot paint outside
  its surface, so no cursor-following ghost beyond bounds is promised;
  armed tear-out shows an explicit in-window indicator at the grab point /
  window edge.
- **Zone geometry**: edge bands one-third of region width/height, center
  the remaining middle; 4 px hysteresis on zone transitions; self-drop and
  center-on-source are no-ops (release = cancel).
- **Ratios**: structural insert/remove re-equalizes all region ratios
  (matches existing `split_workspace`/`remove_workspace` behavior); a
  center swap is a pure leaf exchange preserving tree shape and ratios.
- **Commit ordering**: tree mutation + server transfer/report happen at
  pointer release; settle animation is purely visual afterwards, and the
  reduced-motion zero-duration path yields the identical observable state.
- **Cancel semantics**: Escape is claimed by the chrome during an active
  drag (never reaches the PTY, P5); cancel sends zero protocol frames and
  changes no selection/focus (bypasses the tab-drag click-restore path).
- **Focus rules**: the moved workspace stays focused with its previously
  active tab (no select-first-tab side effect); after tear-out the source
  window keeps the existing `remove_workspace` first-in-tree focus rule.
- **Tear-out geometry (OQ6 closed)**: new window inherits the source
  window's size (Windows Terminal precedent); placement: cursor top-left
  on X11/macOS, compositor-chosen on Wayland.
- **Reduced motion (OQ9 closed — premise was wrong)**: `appearance.animations`
  + `SCRIBE_DISABLE_ANIMATIONS` + 150 ms `MAX_TRANSITION` already exist and
  are reused; no new setting.
- **Perf budget (P4)**: drop-target/ghost update ≤ one 60 Hz frame
  (16.7 ms) input-to-paint, p95, on the dev-hardware profile; verified by
  an instrumented drag probe named in the plan.
- **Beads icon stays its own control**: the drag grab area is the pill
  minus the nested Beads button; Beads click/hover/keyboard behavior is a
  regression criterion.
- **OQ1 spike is a P0 prerequisite bead**: two-window native probe on
  Wayland, X11, macOS verifying drag-move delivery at/beyond edges,
  re-entry, release, Escape, and blur, before gesture code lands.
- **Cross-window merge stays a non-goal (OQ4 closed)**: no existing-window
  destinations in v1 by pointer or palette; recorded in a phase-2 epic
  alongside tab-between-workspace drag and drag re-attach.

### Non-Blocking Observations

- i18n: English literals match current repo practice; don't persist or
  compare display strings.
- Empty workspaces additionally lack a lower-region handle (needs ≥1 tab);
  covered by Q3's answer.
- Badge visibility doc comment says "multi-workspace" while the call
  hardcodes `true`; clean up when touching the badge path.
- "Byte-identical" in US4 to be reworded as "same protocol operations"
  (tear-out mints a fresh `WindowId`).
- Verification deliverables for the plan: per-story user-reachable paths
  (P3) including both badge locations, all zones, cancel/blur/disappear
  races, PTY leakage probe, upgrade-mid-transfer oracle, palette-only run;
  extend existing e2e suites (tab-drag-reorder, workspace-ipc,
  multi-window-restore, server-upgrade-reattach, agent-world) with
  wire/tree oracles over screenshots.
- Docs fallout: `docs/agent-api.md` window-id semantics, `lat.md/`
  client/server/protocol/test sections.
- No packaging impact identified.

## Clarifications

Answers given at the clarify gate (2026-08-23). The technical-decision list
above drew no objections (silence = consent).

**Q1: Which in-window drop model should workspace-pill dragging use?**
A: 5-zone — edge bands split the hovered region, center swaps positions.
Reflected in Goals and US1.

**Q2: What happens when the user drags the pill of a window's only
workspace outward?**
A: No-op with feedback (ghost snaps back with a brief hint). Reflected in
US2.

**Q3: Unnamed workspaces render no badge pill; what's the v1 drag handle?**
A: Neutral fallback pill for every region in multi-region windows; named
pills unchanged. Reflected in Goals and US1.

**Q4: How is tear-out armed during a drag?**
A: Hybrid — at/past the window edge: out-of-bounds coordinates where the
backend delivers them (X11/macOS), inner edge band everywhere (covers
Wayland). Reflected in US2 and C1.

**Q5: How does v1 treat tear-out on shared or remotely-controlled windows?**
A: Allow it; the share stays keyed to the source window and viewers lose
the moved workspace (clean detach, no partial frames, no dangling sinks).
Share migration is phase-2. Reflected in US2 and Non-Goals. (Note: this
overrides the review's recommended local-only restriction.)

**Q6: Which palette/keyboard commands ship in v1?**
A: "Move workspace to new window" plus directional region moves
(left/right/up/down). Reflected in US4.

**Q7: Does animated feedback ship in v1?**
A: Yes — animated zone fade, settle, and snap-back under the existing
`AnimationSettings` policy with the measured frame budget. Reflected in
Goals and US3.

## Architecture Approach

Two distinct mechanisms behind one gesture, split by whether window
ownership changes:

1. **In-window rearrange is client-local.** The drag commit calls new pure
   tree operations (extract / insert-at-edge / swap) defined once on
   `WorkspaceTreeNode` in `scribe-common` and consumed by `WindowLayout`,
   persisting through the existing `ReportWorkspaceTree` path. No new
   protocol. This keeps the hot, latency-sensitive interaction entirely in
   the client (constitution P4) and reuses the persistence semantics every
   other layout edit already has (P2).
2. **Tear-out is one server transaction.** A new capability-gated protocol
   operation (`TransferWorkspace { transfer_id, workspace_id,
   target_window_id }` → typed ACK/refusal) performed on the source
   window's existing connection. The request carries **ids only**: the
   server derives both post-move trees itself using the shared
   `scribe-common` tree operations on its authoritative copy of the source
   window's tree — there is no client-supplied tree to validate or trust.
   The transaction runs under a dedicated **transfer gate** (an async
   mutex) that the handoff snapshotter and agent world capture also
   acquire, with the existing registry lock order preserved inside it
   (live sessions → shares → workspace manager, matching
   `agent_api/world.rs`); fallible I/O (env DEK + envelope copy) is staged
   *before* the commit point, so the in-gate mutation is infallible
   in-memory state. Within the gate the server: validates ownership and
   refusal conditions, reassigns every session's window mapping, rewrites
   both window trees, commits the env-envelope coordinates, detaches the
   moved sessions from the source window's share sinks and every
   remaining source connection's attached-session set, records the result
   in a bounded transfer ledger, and re-broadcasts in-flight agent
   activity leases to the destination. After the gate it pushes refreshed
   session-list/tree state to the remaining source-window connections
   (existing frame types — old viewers need no new protocol) and ACKs the
   requester. Only after the ACK does the client open the target window,
   which claims the freshly minted `WindowId` via the existing
   claimed-window bootstrap (`initial_session: false`, as
   `open_restored_window` does) and adopts the moved sessions through the
   normal attach flow.

Crash-safety falls out of the ordering: after ACK the sessions belong to
the new `WindowId` even if the client dies before opening it — that is
exactly the "window with sessions and no client" state the existing
restore/`other_windows` machinery already recovers. Before the commit
point every failure is a typed refusal with byte-identical server state
(staged env copies are garbage-collected). Retry after a lost ACK hits
the transfer ledger and gets the recorded result instead of a spurious
`not owner` refusal.

The drag gesture itself is a client-side state machine in a new
`workspace_drag` module: pure zone hit-testing (5-zone, thirds, 4 px
hysteresis), arming rules (edge band ∪ out-of-bounds), and drag lifecycle,
unit-testable without GPUI. Chrome integration follows the existing
tab-drag pattern: a dedicated GPUI drag marker type (distinct from
`TabDrag`) with an empty native ghost, the visible pill ghost and zone
overlays painted as deferred elements by the shell.

Alternatives considered and rejected:
- Client-choreographed multi-frame move (`ReportWorkspaceTree` + ad-hoc
  reassignment): rejected — the server actively refuses cross-window
  attach (`filter_attachable_sessions`), and intermediate states would be
  observable by handoff snapshots (violates C4).
- Implementing `xdg_toplevel_drag` / upstream GPUI changes for true
  cursor-following tear-out: non-goal; pinned rev.
- Spawning the torn-out window before the server commit: rejected — a
  refusal would strand an empty window, and the claimed-window path needs
  the sessions already reassigned to adopt them.

## Affected Components

- `crates/scribe-client/src/workspace_drag.rs` (new): drag state machine,
  zone hit-testing, arming/hysteresis logic, pure unit tests.
- `crates/scribe-client/src/titlebar.rs`: pill drag source (top regions),
  neutral fallback pill, drag-marker routing kept distinct from `TabDrag`;
  Beads icon exclusion from the grab area.
- `crates/scribe-client/src/tab_bar.rs`: badge label fallback for unnamed
  workspaces (neutral pill), doc-comment cleanup of the multi-workspace
  claim.
- `crates/scribe-client/src/main.rs`: region-bar pill drag sources
  (`RegionChrome`), standalone region pills for zero-tab/unnamed regions,
  ghost/zone overlay rendering, drag commit dispatch, tear-out flow
  (transfer request, refusal/timeout/late-result handling, target-window
  open with inherited geometry and X11/macOS placement, open-failure
  status — `open_restored_window` today ignores `open_window` failure and
  gains a returned status), palette action wiring, Escape claiming during
  drags, the exhaustive `server_message_variant` match, and `Hello`
  construction for the new capability flag.
- `crates/scribe-common/src/` (new module or `protocol.rs` sibling):
  shared `WorkspaceTreeNode` operations — `extract_workspace`,
  `insert_workspace_at_edge`, `swap_workspaces`, ratio rules (equalize on
  structural change, preserve on swap) — consumed by both client layout
  and server transaction.
- `crates/scribe-client/src/workspace_layout.rs`: `WindowLayout`
  integration of the shared tree ops; focus retention rules.
- `crates/scribe-client/src/command_palette.rs`: five new entries using a
  client-local palette action (not `KeybindingsConfig` actions — no
  `keybindings.rs` or settings surface in v1).
- `crates/scribe-client/src/animation.rs`: (reuse only) zone fade /
  settle / snap-back timings under existing policy.
- `crates/scribe-common/src/protocol.rs`: `TransferWorkspace` request +
  result/refusal variants; `Hello`/`Welcome` `workspace_transfer`
  capability flags (`#[serde(default)]`, matching `pi_provider`/`agent_api`
  precedent).
- `crates/scribe-server/src/workspace_manager.rs`: atomic transfer
  transaction (session reassignment + both trees via shared tree ops),
  invariant checks, bounded transfer-result ledger.
- `crates/scribe-server/src/ipc_server.rs`: transfer handler (transfer
  gate, refusal validation, staged env re-bind, share-sink +
  `AttachedSessionIds` cleanup on remaining source connections,
  post-commit session-list/tree refresh to viewers, activity lease
  re-broadcast, capability negotiation, `requires_window_control` gains
  `TransferWorkspace`), and authoritative session→window ownership checks
  on session-addressed messages so stale attachments cannot mutate moved
  sessions.
- `crates/scribe-server/src/env_store/` (incl. `keystore.rs`): staged
  re-bind — DEKs are keyed by `(WindowId, launch_id)`, so the sequence is
  copy DEK to new coordinates → copy envelope file → commit live-session
  `env_window_id`/persist coordinates inside the gate → delete old DEK +
  file after commit; crash between stages leaves only collectable
  garbage, never a broken restore.
- `crates/scribe-server/src/handoff.rs` + `handoff_tests.rs`: transfer
  gate acquisition before snapshotting; ledger serialization; transfer
  atomicity tests.
- `crates/scribe-test/src/{daemon.rs,ipc_fixtures.rs}`: `Hello`/`Welcome`
  constructor updates for the capability flag.
- `specs/016-gpui-client-rebuild/parity-inventory.md` + ratchets: the
  parity gate enumerates protocol variants exactly; new variants must be
  added there (`just parity-inventory` acceptance).
- `tests/e2e/` + `justfile` inventories: pointer-driven suites live under
  `tests/e2e/visual/` (the func image ships no GPUI client/Xvfb/xdotool;
  the visual image does); server-only wire/tree oracles stay func; new
  scripts registered in the exact `justfile` suite lists.
- `docs/agent-api.md`, `lat.md/{client,server,protocol,test}.md`: document
  window-id freshness on tear-out and the new surfaces.

## Data Model

- **Protocol** (additive, capability-gated — no wire migration):
  - `ClientMessage::TransferWorkspace { transfer_id, workspace_id,
    target_window_id }` — ids only; the server derives both post-move
    trees from its authoritative state via the shared tree ops.
  - `ServerMessage::WorkspaceTransferResult { transfer_id, result }` with
    a typed refusal enum: unknown workspace, not owner / not this
    connection's window, no window control, capability absent,
    sole workspace, target window id already exists (collision with any
    session/tree/share registry), mid-handoff, or pre-commit environment
    DEK/envelope re-bind failure (`EnvironmentRebindFailed`; never generic
    `ServerMessage::Error`).
  - `Hello`/`Welcome` gain `workspace_transfer: bool` (serde-default
    false).
- **Server state**: `window_trees` and `session_to_window` mutate inside
  the transfer gate. One new bounded structure: the **transfer ledger**
  (`transfer_id → result`, most-recent 64 entries), serialized into
  handoff state so an ACK lost across an upgrade still deduplicates the
  retry. Env DEKs and envelopes follow the staged copy → commit → delete
  sequence (see Affected Components); the commit point for all state is
  the in-gate mutation.
- **Client state**: `WorkspaceDrag` state machine (idle → armed →
  dragging{zone} → tear-armed → committing → awaiting-result);
  `WindowLayout` consumes the shared tree ops. Neutral/standalone pill =
  existing `GroupBadge` with a fallback label and muted accent, rendered
  independently of first-tab existence — no new persisted fields.
- **Zone geometry (deterministic)**: for a hovered region, compute the
  cursor's normalized distance to each edge; if the minimum normalized
  edge distance is > 1/3 the cursor is in the center (swap) zone;
  otherwise the zone is the axis with the *smaller* normalized distance,
  horizontal winning exact ties — corners resolve deterministically. Zone
  transitions require 4 px of travel past the boundary (hysteresis).
  Tear-out arming: armed when the cursor is ≤ 8 px from any window edge
  or beyond bounds; disarmed only when it retreats > 24 px inside
  (16 px hysteresis band). The spike may tune the 8/24 px constants;
  defaults are normative until it does.
- **Identity invariants**: `WorkspaceId`, `SessionId`s stable;
  `WindowId` fresh on tear-out; `transfer_id` never reused; ledger
  entries expire only by capacity, never by time.

## API / Interface Changes

- Protocol: the two message variants + capability flags above. Additive;
  old peers never see the frames (client checks `Welcome.workspace_transfer`
  before offering tear-out commit; rearrange works against any server since
  it is client-local + `ReportWorkspaceTree`).
- UI surfaces: pill drag on both bars; zone overlays; tear-out arming
  indicator; five command-palette entries ("Move workspace to new window",
  "Move workspace left/right/up/down") as client-local palette actions —
  not `KeybindingsConfig`/settings-listed actions in v1. Directional
  semantics: nearest neighbor via the existing directional scoring
  *without wrap* (today's `find_workspace_in_direction` wraps — the
  command variant must not); the focused workspace is extracted and
  inserted at the neighbor's far edge in the travel direction (focused B
  in [A|B] moving left lands left of A → [B|A]); no neighbor on that side
  ⇒ no-op with feedback. No settings-window changes.
- Authorization: `TransferWorkspace` joins `requires_window_control`;
  only the source window's controlling connection may transfer. Remaining
  source-window connections receive post-commit session-list/tree
  refreshes (existing frame types, so mixed-version viewers degrade
  gracefully) and their `AttachedSessionIds` entries for moved sessions
  are cleared; session-addressed messages re-validate authoritative
  ownership so a stale viewer cannot key/resize/close a moved session.
- Agent API: no new automation actions in v1. `docs/agent-api.md` gains a
  note that tear-out mints a fresh window id and that `world`/`siblings`
  reflect the move atomically (agents must not cache window ids across
  snapshots — existing guidance, now with a concrete cause).
- Breaking changes: none. Old client + new server: no capability sent →
  server never advertises; drag tear-out hidden. New client + old server:
  `Welcome` lacks the flag → tear-out commit refused client-side with the
  disabled-UX message; rearrange unaffected.

## Testing Strategy

- **Unit (common, pure)**: shared tree ops — extract/insert/swap on
  nested trees, ratio equalize-vs-preserve, degenerate single-leaf
  refusal — exercised once, used by both peers.
- **Unit (client, pure)**: zone resolution across region rects including
  every corner (normalized-distance precedence, horizontal tie-break),
  4 px zone hysteresis, arming rule (8 px band, out-of-bounds, 24 px
  disarm), drag state machine transitions (cancel/Escape/blur/
  workspace-disappear → zero-commit, next drag immediately valid),
  directional-command neighbor selection (no wrap, no-neighbor no-op),
  focus retention, neutral/standalone pill fallback data (named, unnamed,
  zero-tab, top and lower bars).
- **Unit (protocol)**: serde round-trip of new variants; capability
  default-false from old peers (mirrors existing capability tests).
- **Server tests**: transfer transaction — success reassigns every
  session and both trees atomically (server-derived trees match shared
  tree-op output); every refusal path leaves state byte-identical
  (including target-id collision and mid-handoff); ledger retry — lost
  ACK then retry returns the recorded result, before and after a handoff;
  staged env re-bind — DEK copy + envelope copy + coordinate commit +
  old-key deletion, with injected failure at each stage leaving restore
  intact; share-sink detach and `AttachedSessionIds` cleanup — an
  attached viewer connection receives the post-commit refresh and can no
  longer key/resize/close moved sessions (ownership enforcement); source
  controller likewise loses moved-session addressing; activity lease
  re-broadcast; handoff snapshot taken while a transfer holds the gate is
  strictly pre- or post-state (extend `handoff_tests.rs`).
- **E2E visual (X11/Xvfb — pointer-driven suites live here; the func
  image ships no GPUI client)**: scripted xdotool drags with measured
  offsets (per learnings): rearrange swap + each edge insert with
  wire/tree oracles; corner-zone determinism probe; tear-out producing a
  second window whose sessions are the same server sessions (ids stable,
  running program still alive, active tab and pane trees preserved, no
  `SessionCreated` observed — no login shell); X11 placement oracle
  (top-left within tolerance of release point); cancel via Escape leaves
  zero tree/report diff and zero focus change; disappear-mid-drag
  (workspace closed under the drag) cleans up; PTY leakage probe
  (mouse-reporting app sees no gesture or Escape bytes); zone overlay
  geometry against code-derived baselines with animations disabled
  (zero-duration end state equals animated end state); neutral pill
  appearance; chrome regression matrix — tab drag-reorder (top + lower
  bars), pill click-to-focus, Beads icon click/hover, empty-chrome window
  move all unchanged.
- **E2E func (server-only oracles)**: reconnect/restart persistence of a
  rearranged tree; upgrade-mid-transfer oracle in the
  server-upgrade-reattach family; mixed-version integration (old client +
  new server, new client + old server — capability negotiation and
  disabled UX); agent-world window-id assertions (fresh id, atomic flip).
- **Palette-only run** (P3 keyboard path): every outcome — four
  directional moves incl. no-neighbor no-op, and move-to-new-window —
  driven without pointer input.
- **Wayland/macOS evidence (P3, C2)**: the platform spike's probe binary
  *plus* a post-implementation scripted manual checklist covering the
  completed US1/US2 outcomes (drag, zones, arm at edge, release, tear-out
  window contents, Escape, blur) recorded in the closure bead; no CI
  automation exists for these backends.
- **Performance (P4, named harness)**: `SCRIBE_DRAG_PROBE=1` enables
  tracing spans from pointer-event ingestion to overlay paint; the
  scripted drag in `tests/e2e/visual/` extracts the distribution from the
  client log and the closure bead records p95 ≤ 16.7 ms on the
  documented dev-hardware profile (CI runs the script functionally
  without asserting the timing budget — Lavapipe is not the profile).
- **Repo gates as acceptance**: `just ready`, `just parity-inventory`
  (new protocol variants enumerated), `just reachability`, and
  `lat check` — sequenced in the closure work item.

## Risks

- **Platform input delivery (highest)**: if the spike shows Wayland stops
  motion before the inner edge band is reachable, arming degrades; the
  band is inside the window so this is unlikely, but the spike gates all
  gesture work. Mitigation: band-only arming works even with zero
  out-of-bounds delivery.
- **GPUI deferred-overlay behavior** at window edges (ghost clipping,
  hitbox occlusion): learnings show GPUI layout/hover chains fail silently
  — mitigation: visual e2e measures the overlay, and zone hover is
  computed level-triggered from the drag position every frame, never from
  GPUI hover state (edge-triggered-cache learning).
- **Env envelope re-bind**: DEKs are keyed by `(WindowId, launch_id)` and
  persist tasks capture window coordinates — wrong staging silently
  breaks cold restore for moved sessions; mitigation: the staged
  copy→commit→delete design with per-stage failure-injection tests and a
  restore verification.
- **Share/attachment cleanup races**: viewers of a shared source window
  must see a clean refresh and lose addressing for moved sessions;
  mitigation: server test with an attached viewer connection asserting
  both the refresh frames and the ownership refusals.
- **Handoff mid-transfer**: handoff snapshots live sessions and workspace
  state separately today; mitigation: the transfer gate is acquired by
  the snapshotter, the transaction, and world capture alike, preserving
  the existing live→shares→workspace lock order inside it — plus a
  handoff test holding the gate (this is the C4 invariant).
- **xdotool drag flakiness** in e2e: use measured offsets and the existing
  tab-drag suite's conventions; keep func oracles on wire/tree state, not
  pixels.
- **Perf budget under Lavapipe** (software Vulkan in CI): the 16.7 ms p95
  target is for the dev-hardware profile; CI asserts functional frames
  only, the probe runs on dev hardware (documented in the probe bead).
- **Client error paths**: refusal, request timeout/disconnect,
  late/duplicate results, and target-window open failure each need
  explicit handling; `open_restored_window` today ignores `open_window`
  failure — the tear-out path returns a status and surfaces the
  recoverable state (sessions safe under the new id, restored-window
  offer). Mitigation: dedicated acceptance criteria on the client
  tear-out item.
- **Rollback**: all client work is behind the gesture (no config
  migration); protocol additions are additive and capability-gated —
  reverting the server keeps old clients working by construction.

## Sequencing

Shared primitives extracted first. Several client items all modify
`crates/scribe-client/src/main.rs` (and chrome files), so the client-side
chain is deliberately serialized — parallel workers branch from main and
cannot see each other's unlanded edits. The server-side chain shares no
files with the client chain and runs in parallel to it. Order is
expressed as blocking edges (no step codes).

- **Platform input spike** (P0; blocks: drag core, client tear-out) —
  probe binary opening two GPUI windows; verify drag-move delivery
  at/past edges, re-entry, release, Escape, blur on Wayland, X11, macOS;
  record findings + confirmed 8/24 px arming constants in the spec and
  `lat.md/client.md`.
- **Shared tree operations in scribe-common** (P0; blocks: server
  transaction, drag core, palette) — `extract_workspace`,
  `insert_workspace_at_edge`, `swap_workspaces` on `WorkspaceTreeNode`,
  ratio rules, sole-leaf refusal; pure unit tests.
- **Protocol transfer surface** (P0; blocks: server transaction, client
  protocol wiring) — ids-only request, result + full refusal enum,
  capability flags, serde tests, parity-inventory rows.
- **Server transfer transaction** (P1; needs: shared tree ops, protocol
  surface; blocks: client tear-out, closure) — transfer gate shared with
  handoff/world capture, refusal validation, in-gate commit, transfer
  ledger (+ handoff serialization), staged env DEK/envelope re-bind,
  share-sink + `AttachedSessionIds` cleanup, post-commit viewer refresh,
  ownership enforcement on session-addressed messages, lease
  re-broadcast; server tests incl. handoff-gate and per-stage env
  failure injection. Server files only — parallel to the client chain.
- **Client protocol wiring** (P1; needs: protocol surface; blocks:
  standalone pills [main.rs chain]) — `server_message_variant` exhaustive
  match, `Hello` capability, `Welcome` capability plumbing to UI state,
  scribe-test fixture constructors.
- **Standalone region pills** (P1; needs: client protocol wiring [chain
  order only]; blocks: drag core) — every region gets a pill independent
  of naming and first-tab existence, on both bars; muted accent for
  unnamed; doc-comment cleanup; unit data tests.
- **Drag core & in-window rearrange** (P1; needs: spike, shared tree ops,
  standalone pills; blocks: client tear-out) — `workspace_drag` module
  (state machine, corner-deterministic zones, hysteresis, arming), pill
  drag sources on both bars, ghost + zone overlays, commit +
  `ReportWorkspaceTree`, cancel/Escape/blur/disappear claiming,
  PTY-leakage guard; unit tests.
- **Client tear-out flow** (P1; needs: drag core, server transaction;
  blocks: palette) — arming UI, transfer request/result handling incl.
  every refusal, timeout/disconnect, late/duplicate result, open-failure
  status + recovery surfacing; target window bootstrap (claimed id,
  `initial_session: false`, inherited size, X11/macOS cursor placement),
  sole-workspace no-op feedback, capability-absent disabled UX; unit
  tests (E2E lands in closure).
- **Palette commands** (P2; needs: client tear-out [chain], shared tree
  ops) — five client-local palette actions; no-wrap neighbor selection,
  far-edge insertion, no-neighbor no-op; unit tests.
- **Animation polish + perf probe** (P2; needs: palette [chain order —
  last main.rs writer]) — zone fade, settle, snap-back under
  `AnimationSettings`; `SCRIBE_DRAG_PROBE=1` spans; probe script.
- **Verification & docs closure** (P2; needs: server transaction, client
  tear-out, palette, animation) — visual-suite pointer tests (xdotool,
  measured offsets, justfile inventory registration), func server-only
  oracles (reconnect, upgrade-mid-transfer, mixed-version), agent-world
  window-id assertions, chrome regression matrix, palette-only run,
  Wayland/macOS manual checklist, p95 record on dev profile,
  `docs/agent-api.md` note, `lat.md/` sync, and the repo gates
  (`just ready`, `just parity-inventory`, `just reachability`,
  `lat check`). Also records the phase-2 epic (cross-window merge,
  tab-between-workspace drag, drag re-attach, share migration).

## Normative Visual Coverage

None (0 rows). Source Authority contains a single reference-only
screenshot; no normative visual artifact exists for this feature.

| Row / artifact locator | Normative requirement | Goal / Non-Goal alignment | Implementation work item | Verification work item + oracle | Status |
|------------------------|-----------------------|---------------------------|--------------------------|---------------------------------|--------|
| —                      | —                     | —                         | —                        | —                               | —      |

## Backlog Refinement

None — the Backlog Inputs section records no P4 sources (none supplied,
none found in the open backlog). No dispositions required.

## Constitution Check (plan)

- **P1**: transfer is one typed operation with typed refusals; crate
  boundaries preserved (pure logic in client modules, transaction in
  server, shared types in common). No new dependencies.
- **P2**: sessions never restart; ratios/focus follow existing rules
  (equalize on structural change, first-in-tree source focus); chrome
  regressions (tab drag, pill click, Beads icon, window move) are explicit
  test criteria.
- **P3**: every story has a named user-reachable path (visual e2e for
  US1/US2 on X11, post-implementation scripted checklist for
  Wayland/macOS, palette-only run for US4, leakage probe for US3).
- **P4**: numeric budget (16.7 ms p95 input-to-paint) with a named
  harness (`SCRIBE_DRAG_PROBE=1` + visual-suite drag script + documented
  dev-hardware profile); animations capped at 150 ms with the
  zero-duration path asserted equal.
- **P5**: whole gesture claimed including Escape; no new capability
  crosses the PTY trust boundary.
- **P6**: no network surface; fully local.
- **P7**: capability-gated additive protocol; mixed-version behavior
  specified; lat.md sync + `lat check` is a sequenced work item; the spike
  runs dev instances only (no live `--upgrade`).

Learnings-store check: the gesture-claiming, measured-offset e2e,
silent-GPUI-layout, and edge-triggered-hover-cache learnings are each
reflected in Testing Strategy/Risks above; no plan item re-attempts a
documented failed approach.

## Alignment fixes applied

Two-subagent alignment round (A: source/spec↔plan, B: plan quality);
every must-fix applied:

- (A/B, must) Commit ordering vs rollback contradiction resolved: US2 AC
  now states the pre-commit byte-identical / post-ACK committed-recovery
  contract; architecture documents the ledger-backed retry.
- (A/B, must) Atomicity made feasible: dedicated transfer gate shared by
  the transaction, handoff snapshotter, and agent world capture;
  live→shares→workspace lock order preserved; fallible env I/O staged
  pre-commit.
- (A/B, must) Env re-bind fully specified: DEKs keyed by
  `(WindowId, launch_id)` — staged copy DEK → copy envelope → in-gate
  coordinate commit → post-commit deletion, with per-stage
  failure-injection tests.
- (A/B, must) Untrusted-tree surface removed: request is ids-only; the
  server derives both trees via shared scribe-common tree operations;
  refusal enum extended (target collision, window control, mid-handoff).
- (A/B, must) Idempotency backed by a bounded transfer ledger serialized
  into handoff state; lost-ACK retry tests before/after handoff.
- (A/B, must) Shared-viewer design completed: post-commit session-list/
  tree refresh over existing frames, `AttachedSessionIds` cleanup,
  `requires_window_control` gains `TransferWorkspace`, authoritative
  ownership enforced on session-addressed messages, with tests.
- (A, must) 5-zone corner determinism: normalized-distance precedence
  with horizontal tie-break; numeric arming hysteresis (8 px arm /
  24 px disarm) pinned as normative defaults.
- (A, must) Directional command semantics defined: no-wrap neighbor
  scoring, far-edge insertion, no-neighbor no-op with feedback.
- (A/B, must) Verification gaps closed: named P4 harness
  (`SCRIBE_DRAG_PROBE=1` + visual drag script + dev profile), reconnect /
  no-`SessionCreated` / active-tab+pane-tree / placement / mixed-version
  / disappear-mid-drag / chrome-regression oracles added; repo gates
  (`just ready`, `just parity-inventory`, `just reachability`,
  `lat check`) sequenced; Wayland/macOS checklist now covers completed
  US1/US2 outcomes.
- (A, must) Standalone region pills specified independent of first-tab
  existence (empty top regions included).
- (B, must) Affected components completed: `server_message_variant`
  match, Hello/Welcome constructors (client + scribe-test fixtures),
  parity-inventory rows, justfile suite inventories; palette entries use
  client-local actions (no keybindings.rs).
- (B, must) main.rs sequencing serialized: protocol wiring → standalone
  pills → drag core → tear-out → palette → animation; server chain
  parallel; tear-out E2E split into closure.
- (B, must) Pointer-driven suites moved to the visual e2e image (func
  image ships no GPUI client/Xvfb/xdotool).
- (A/B, should) "Approved screenshots" → code-derived baselines; client
  error-path acceptance (refusals, timeout, late/duplicate results,
  open-failure status) added; phase-2 epic recording sequenced in
  closure.
- Human-authority blockers: none — no normative visual artifact exists
  and no Non-Goal conflict was found (alignment ledger: zero Non-Goal
  violations).
