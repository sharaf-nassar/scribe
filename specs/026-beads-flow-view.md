# beads-flow-view

## Status

Draft. Awaiting the speckit clarify gate.

The normative visual reference for every surface in this document is
`.impeccable/mocks/beads-board-directions.html`, section **A3 · Flow**. That
file is the approved design; this spec exists to reproduce it pixel for pixel
in GPUI, not to reinterpret it. Where prose and mock disagree, the mock wins
and the prose is the bug.

## Problem Statement

The pinned Beads board renders five status buckets. Beads is not a bucket
tracker — it is a dependency graph. `bd ready` is defined as "open issues with
no active blockers", and the shape a developer actually needs to see is which
work released which, what is running now, and what the next hop is. Five
columns cannot express that: the current board shows `2a8z.4` and `2a8z.5` as
two unordered rows in one lane and says nothing about the fact that they are
parallel branches converging on `2a8z.6`.

Opening a card today raises a read-only detail panel over the terminal. It
answers "what is this issue" but not "why can I not start it" — the reader gets
a blocker list, which is a textual encoding of a position they could simply be
shown.

This spec adds a second rendering of the same board — **Flow** — that ranks
issues by depth in the blocker DAG, and makes opening a card the thing that
switches into it, with the opened card marked. Clicking any other node moves
the opened card without leaving the view.

## Goals

- Opening a Beads card switches that workspace's board into Flow for the
  card's epic, with the opened card rendered as the cursor node, matching the
  mock's `Opened "Document Pi integration" from the Blocked lane` state.
- Clicking any other node in Flow makes that node the cursor node and updates
  whatever detail surface is open, without leaving Flow and without a
  full-board reload.
- Flow ranks nodes by depth in the blocker DAG: rank 0 holds nodes with no
  blocker inside the epic, and each subsequent rank holds nodes whose blockers
  all resolve to an earlier rank.
- Node state reads at a glance from one 8px dot: done filled, ready hollow,
  blocked hollow, and the node an AI agent is running **in this window**
  carries a halo. The halo arrives on a `issue_focused` hook event emitted by
  the Scribe Pi extension (Q2) and is sequenced after the graph; Flow degrades
  to ordinary state treatment without it.
- Hovering a node traces its path — ancestors and descendants stay at full
  opacity, everything else drops to `opacity: 0.24`, and the traced wires go
  to `#e9ebf0` — matching the mock's second state.
- Returning to Lanes is one control, and the round trip preserves the board's
  pin, height, and text scale.
- Every painted value matches the mock within the tolerance stated in
  [Normative visual reference](#normative-visual-reference).

## Non-Goals

- No new tracker capability: Flow reads and highlights, it does not create,
  reorder, or re-parent issues.
- No graph editing — dependencies are not drawn, cut, or rewired from Flow.
- No replacement of the Lanes board. Lanes remains the default rendering and
  keeps the drag contract described in
  [[client#Client#Beads Board CLI Data Source#Board interaction and issue detail]].
- No multi-epic canvas. Flow shows exactly one epic at a time.
- No curved edges, node dragging, zoom, or pan. GPUI has no bezier primitive
  and the strip is 197px tall; edges are orthogonal 1px rects.
- No change to the guarded write path, the detail panel, or the undo deadline.
  The panel opens under the board exactly as it does today.
- No Flow for an issue with no epic or no graph — those cards stay in Lanes.
  Cycles, disconnected nodes, and cross-epic blockers are therefore admission
  failures, not layouts.
- No epic switcher. The band chevron is inert until it has an action.
- No Flow in shared or remote windows; it rides the local-owner detail gate.
- No vertical scrollbar in any Flow state.
- No keyboard graph traversal beyond the AccessKit contract, no search, and no
  centre-on-node — likely next requests, explicitly out of this feature.

## Backlog Inputs

None. No `source_backlog` was supplied, and no open P4 issue exists in the
tracker at the time of drafting (`bd list --status=open --priority=4` is
empty), so there is no refinement obligation attached to this run.

## Target Epic

This run creates a new epic. No open epic covers the Beads board:
`scribe-5wh1` (beads card detail) is closed, and `scribe-2a8z`
(pi-ai-integration) is unrelated. The clarify gate did not name an alternative,
so the ambiguity is resolved in favour of a new `beads-flow-view` epic.

## User Stories

### Story 1 — Opening a card lands in Flow

As a developer glancing at the pinned board, I want clicking a card to show me
that card's epic as a graph with my card marked, so that "why can I not start
this?" is answered by position instead of by reading a blocker list.

Acceptance criteria:

- Clicking any card in Lanes opens the detail panel under the board exactly as
  it does today, **and** switches that workspace's board to Flow for the clicked
  card's epic, with the clicked issue rendered as the cursor node. The panel is
  unchanged; the strip is what swaps.
- The cursor node paints exactly the mock's `.fl .node.cursor` treatment:
  background `#ffffff0f` and a 2px inset keyline in `#e9ebf0` on its left edge.
  No other node in the view carries that treatment.
- The band names the epic, its `closed/total` tally, a progress bar filled to
  the closed fraction, and the opened issue id, laid out left to right exactly
  as the mock's first Flow state.
- Only the clicked workspace's board changes mode. A second region showing its
  own board is untouched.
- Switching to Flow does not change the board's pinned state, its per-workspace
  height, or the window's text scale.
- A card whose issue has no epic, or whose epic yields no graph, does not switch
  modes: the board stays in Lanes and only the panel opens. No empty Flow frame
  is ever painted.

### Story 2 — Move the cursor by clicking another node

As a developer reading the graph, I want to click a different node to make it
the opened issue, so that I can walk a chain without going back to Lanes.

Acceptance criteria:

- Clicking a non-cursor node makes it the cursor node and retargets the open
  panel to that issue; the previous cursor node returns to its ordinary state
  treatment in the same frame.
- **The board does not swap.** A node click changes which issue is open, never
  which epic the graph shows. Only a Lanes card click swaps the epic.
- The band's opened-issue id updates to the newly clicked node.
- The view does not leave Flow, does not re-rank, and does not re-fetch the
  epic graph — the ranks and wires are identical before and after.
- If a detail surface is open for the previous issue, it retargets to the new
  one; a reply for the previous issue that arrives after the click is
  discarded, not painted.
- Clicking the cursor node again is a no-op, not a toggle back to Lanes.

### Story 3 — Read state and liveness from the node

As a developer, I want each node to tell me its state and whether a machine is
on it right now, so that the graph reports observed truth rather than what
someone remembered to set.

Acceptance criteria:

- Done nodes paint a filled dot in the done hue, title at `#949ca8` weight 550,
  and priority glyph at `opacity: 0.6` — the mock's `.fl .node.done`.
- Ready nodes paint a hollow dot: `inset 0 0 0 1.5px` in the ready hue.
- Blocked nodes paint a hollow dot in the blocked hue with the title lifted to
  `#cdd3dd`.
- A node whose assignee resolves to an AI session live in this window paints
  the live treatment: filled dot in the progress hue with a 3px halo at 20%
  of that hue, title `#f4f6fa` weight 650, and an agent line in `#9fb0ff`
  followed by a 4px status dot.
- A node assigned to an agent that is **not** live in this window does not get
  the halo. Assignment alone is not liveness.
- Absent assignee data degrades to the ordinary state treatment with no agent
  line and no notice.

### Story 4 — Trace a chain by hovering

As a developer, I want hovering a node to show me only its chain, so that I can
see what it waited on and what it releases in a graph with parallel branches.

Acceptance criteria:

- Hovering a node drops every node not on its ancestor-or-descendant path to
  `opacity: 0.24` and leaves on-path nodes at full opacity.
- Wires on the traced path paint `#e9ebf0`; every other wire drops to
  `#ffffff14`.
- The hovered node additionally carries the cursor treatment while hovered.
- A chip near the hovered node states its edge counts in the mock's wording
  and style (`releases 3 · blocked by 1`, `.fl .unlocks`).
- Leaving the node restores every node and wire in one frame.
- Reduced-motion settings do not change the final frame.

### Story 5 — Return to Lanes

As a developer, I want one control to get back to the queues, so that the mode
switch is reversible without hunting.

Acceptance criteria:

- The band carries a back control and a `lanes | flow` mode pair, both styled
  as the mock's `.fl .back` and `.fl .modes`.
- Activating either returns the board to Lanes with its pin, height, scroll
  position, and text scale intact.
- Esc from Flow returns to Lanes when the panel does not have focus.
- The mode does not survive a window restart; it follows text scale rather than
  pin.
- The wheel over the graph scrolls it horizontally. Flow never grows a vertical
  scrollbar: ranks that do not fit vertically are what the compact node budget
  exists to prevent.

## Constraints

**The mock is the contract.** `.impeccable/mocks/beads-board-directions.html`
section A3 supplies structure, sizes, weights, and states. Colour is the one
axis that is *derived*, not copied: the board reads the live theme through
[[crates/scribe-client/src/beads_board.rs#BeadsBoardColors#from_theme]], and
`tests/e2e/visual/beads-board.sh` fails any hardcoded board colour. The mock's
literal hexes are therefore the *reference rendering under the mock's own
theme*; the implementation reproduces their role and relative contrast through
the existing solver, as the Lanes board already does.

**GPUI vocabulary.** Flex/grid divs, 1px rects, two-stop linear gradients,
rounded rects, and text. No bezier paths, no text rotation, no SVG. Edges are
orthogonal 1px rects — a horizontal stub out of the source, a vertical run in
the gutter, a horizontal stub into the target. The mock's wire geometry is
literal and implementable as-is.

**The graph is already in the response the board reads — and is thrown away.**
Re-measured on `bd version 1.1.0 (dev)`, the version `docker/Dockerfile.func`
pins:

- `bd list --all --limit 0 --skip-labels --sort created --json` — the query
  [[crates/scribe-server/src/beads_board.rs#load_board]] already runs on every
  refresh — returns, per issue, `parent` (the epic id) and a `dependencies`
  array of `{ type, depends_on_id }`, where `type` is `parent-child` or
  `blocks`. **Satisfied edges are included**: `scribe-2a8z.2` still reports
  `blocks -> scribe-2a8z.1` with `.1` long closed.
- `bd blocked --json` returns `blocked_by` for *open* blockers only.
- `bd show <id> --json` and `bd dep tree <epic> --json` also carry the graph,
  at the cost of an extra subprocess each.

An earlier draft of this section claimed `list` carried neither field. That was
a measurement error — both are `omitempty`, so an issue with no parent and no
dependencies omits them, and sampling the first record's keys hid them. The
corrected reading is load-bearing: **Flow needs no new `bd` invocation, no
`bd dep tree`, no second round trip, and no separate cache.**

What is lost is in Scribe's own translation. `classify_snapshot` rebuilds
`blocker_ids` from the `bd blocked` response (open blockers only) and reduces
`parent` to a display *name* via `epic_names`, discarding the id. Flow therefore
needs two additive fields on the existing snapshot — a parent epic id and the
typed `blocks` edges as `bd` reported them — not a new read path. The client
still never learns `bd` argv, per
[[protocol#Protocol#Client Messages#Beads issue detail]].

**Strip budget.** 197px reserved, resizable per workspace, text scale 0.8–1.6.
The mock spends 34px on the band, 15px on the rank ruler, and the remainder on
nodes; a 40px node with the mock's rank spacing fits five ranks across a
1558px strip.

**Existing contracts that must not regress.** Lanes keeps its five drop
targets and the Backlog/Blocked rejection rules; the board still polls at 60s
while focused; `NotDetected` still clears the workspace's board; the detail
capability gate in `on_welcome` still governs whether a card may open at all.

**Constitution.** Principle 1 (typed failure, existing abstractions) forces the
graph read through the existing server-owned cache rather than a new client
subprocess path. Principle 2 (consistent UX) is why the mode toggle reuses the
bead's existing hover/pin idiom instead of new chrome. Principle 3 requires each
story to be reachable and verified independently. Principle 4 requires a stated
budget for the graph read and the per-frame node build. Principle 7 requires
`lat.md/` to be updated with the new mode and its contract.

## Normative visual reference

Pixel-perfect replication is an acceptance condition, so the mock's values are
reproduced here as the checkable list. Every number is read from
`.impeccable/mocks/beads-board-directions.html`; the mock remains authoritative
if this table ever drifts.

**Band** (`.fl .band`) — 34px tall, `#ffffff09` fill, 1px bottom border in the
strong hairline, 14px left / 10px right padding, 10px gap. Contents in order:
back control (`.fl .back`, 9px/700 uppercase, `.1em` tracking, `#8d94a1`);
epic name (`.fl .epic`, 9.5px/700 uppercase, `.13em`, `#e7eaf0`); switch
chevron (`#6b7280`); tally (`.fl .tally`, 17px/600 mono, `-.035em`, `#e9ebf0`,
with `/total` at 9.5px `#69707d`); progress bar (150×2px, `#ffffff14` track,
done-hue fill); opened tag (9.5px mono `#69707d`); mode pair right-aligned
(9px/600 uppercase `.1em`, active `#e9ebf0` on `#ffffff12`).

**Rank ruler** (`.fl .rank-ruler`) — 15px tall directly under the band, labels
at 9.5px/500 mono, `.13em`, `#5b626f`, reading `SHIPPED`, `NOW`, `NEXT`
positioned over their ranks.

**Node** (`.fl .node`) — 262×40px. Top line 19px: 8px dot, 7px gap, priority
glyph 9.5px/700 mono `.03em`, title 12px/600 `-.008em` `#e9ebf0` ellipsised.
Sub line 15px, indented 15px, 3px below: id 9.5px/500 mono `#7a828f`, middot
separators `#525a67` with 6px margins, timestamp `#5f6673`. Hover
`#ffffff08`.

**Wires** (`.fl .wires i`) — 1px rects, `#ffffff38`. Traced: on-path `#e9ebf0`,
off-path `#ffffff14`.

**States** — as enumerated in Story 3, plus cursor (`#ffffff0f` fill, 2px inset
`#e9ebf0` left keyline) and trace dim (`opacity: 0.24`).

**Chip** (`.fl .unlocks`) — 3px/7px padding, 2px radius, `#20242b` on a
`#ffffff2b` hairline, 9.5px/500 mono `#c9ced8`.

**Floor** (`.fl .floor`) — 3px `#ffffff0f` with a centred 34×1px grip mark in
`#ffffff2b`.

Tolerance: geometry exact at scale 1.0; text metrics may differ by the
font-stack delta between the mock's web stack and the client's UI font, which
the visual contract must measure as box positions rather than glyph rasters.

## Open Questions

1. **Does Flow replace or accompany the detail panel?** Story 1 currently
   assumes Flow is what a card open lands on. The panel is a shipped feature
   with its own visual contract; retiring it, keeping both, or making the panel
   open *from* Flow are three different scopes.
2. **What happens to the five drag targets while Flow is showing?** Blocked and
   Done never arm a source, so a Flow-mode drag would need new semantics or to
   be disabled. Disabling silently loses a shipped gesture.
3. **What does a card with no epic do?** Standalone issues exist
   (`scribe-cxeh` closed a bug about them). Options: stay in Lanes and open the
   panel, or render a single-node Flow.
4. **Does the mode persist?** Text scale is per window and dies with it; pin
   rides the window geometry record. Flow could follow either.
5. **How does a graph wider than the strip scroll?** Five ranks fit; a deeper
   epic does not. Horizontal scroll, rank collapsing, and windowing around the
   cursor are all candidates.
6. **How is "running here" resolved?** `bd` stores an assignee string; Scribe
   knows its own sessions' providers and task labels. The join is unspecified
   and may need a new field on `BeadsBoardItem` or the graph response.
7. **How is the epic graph fetched and cached?** One `bd dep tree <epic>` per
   open, cached beside the board snapshot, is the cheap answer; refresh cadence
   and staleness behaviour are undecided.
8. **Do closed issues outside the epic appear?** The mock shows only
   epic-internal nodes; a blocker living in another epic has no rank.
9. **Rank assignment for cycles.** `bd dep cycles` exists, so cycles are
   possible. Undefined today.
10. **Does the board still poll while Flow is open**, and does a refresh that
    changes the graph re-rank under the reader's cursor?

## Constitution check

No principle is violated by the draft. Principle 3 is the one at risk: five
stories each need an independently reachable verification path, and the visual
contract must not site its assertions where clamping hides the property — the
recorded failure in
`docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md` is
exactly this board's tooltip contract passing with the bug present. Flow's node
and wire assertions must be sited on interior ranks, not on the first or last.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility, scope,
stakeholders) against the draft, the constitution, the normative mock, and the
shipped board code. Findings merged; cross-dimension hits ranked first.

### Critical Questions (answer before planning)

1. **Does Flow replace the detail panel on card click, accompany it, or open
   from it?** The draft's Goals assert the switch as fact while Open Question 1
   calls it unresolved — the spec contradicts itself. This is the single
   largest scope fork: the shipped panel is not read-only, it carries guarded
   edits, comments, status changes, pickers and a five-second undo
   ([[client#Client#Beads Board CLI Data Source#Guarded issue writes]]), and
   `tests/e2e/func/beads-board.sh:532` asserts a sub-2px card click sends
   `RequestBeadsIssueDetail` with ~200 lines of panel proof behind it.
   Recommended: **preserve and retarget the panel**; Flow changes what the
   *strip* shows, not what the click opens. Flagged by: requirements, gaps,
   ambiguity, feasibility, scope, stakeholders.
2. **Is the live-agent halo in v1, or phase 2?** It is the feature's headline
   differentiator and it has no data path: `BeadsBoardItem` has no assignee,
   and real assignees in this repo are run slugs
   (`codex-implement-ready-run-20260731T071807.2chlr1`) while `SessionInfo`
   carries only provider and a free-text task label — nothing bound to an issue
   id. Shipping it needs an assignee field plus an agreed join convention.
   Flagged by: requirements, gaps, ambiguity, feasibility, scope.
3. **What does Flow do with a graph it cannot rank?** Three cases, one answer
   needed: a card with no epic, a node whose blockers live outside the epic
   (today `classify_issue` calls it Blocked while the rank rule would place it
   at rank 0 under `SHIPPED` — the dot and the position would contradict), and
   a cycle (`bd dep cycles` exists, so this is real). Recommended: stay in
   Lanes and open the panel for the no-epic case; render external blockers as
   an off-graph marker; park cycle members at max rank. Flagged by:
   requirements, gaps, ambiguity, scope.
4. **How does a graph that does not fit behave?** Two axes, both unaddressed.
   Depth: five ranks need ~1460px at the mock's 286px pitch, but boards are
   region-scoped ([[crates/scribe-client/src/pane_shell.rs#PaneShell#board_rect]]),
   so a side-by-side split affords about two. Width: only two 40px rows fit the
   145px node band, so a rank with three siblings overflows today. Horizontal
   scroll, rank collapsing, and windowing around the cursor are different
   features. Flagged by: requirements, gaps, feasibility, scope.
5. **Is the graph frozen at open, or live?** The board polls every 60s while
   focused. Re-ranking under the reader's cursor mid-read is a real behaviour
   choice, and freezing invites a staleness indicator instead. Recommended:
   **frozen per open**, refreshed on an explicit action. Flagged by:
   requirements, gaps, scope.
6. **Does Flow work in shared and remote windows?**
   [[crates/scribe-server/src/ipc_server.rs#beads_detail_connection_available]]
   refuses detail reads for any remote connection and for any local connection
   once a window leaves `SingleController`. If the graph rides the detail gate,
   Flow silently disappears for shared sessions; if it rides the board gate, a
   remote viewer sees the DAG. Flagged by: gaps, stakeholders.
7. **Does the epic chevron do anything?** The mock styles `.fl .switch` as
   pointer-interactive and this spec calls it a switch, but no story defines
   it. An epic picker means discovery, selection state, and more reads.
   Recommended: **inert in v1**, and say so in Non-Goals. Flagged by:
   ambiguity, scope, gaps.

### Technical Decisions (self-resolved — veto at the gate to override)

- **Graph source: extend the existing snapshot, add no `bd` call.** Re-measured
  `bd list --all --json` already returns `parent` and typed `dependencies`
  including satisfied `blocks` edges; the server discards both. Add
  `parent_epic_id` and typed edges to `BeadsBoardItem` rather than introducing
  `bd dep tree`, a second round trip, or a second cache. This also keeps the
  graph inside `BeadsBoardCache`'s existing generation fence, so an applied
  write cannot leave Flow stale — the failure mode a separate `(root, epic)`
  cache would have introduced.
- **Ranking: longest path.** `rank(v) = 1 + max(rank(u))` over in-epic `blocks`
  predecessors, 0 when none. The draft's wording admitted both longest-path and
  earliest-feasible; longest-path is the Sugiyama-standard layering and is what
  the mock draws.
- **Cycles: iterative relaxation with a visited set**, parking unresolved
  members at `max_rank + 1` and painting back edges like any other wire. Cheap,
  total, and removes an open question.
- **Within-rank order: one barycenter pass** over predecessor positions, ties
  broken by creation order. The mock's rank 2 puts `.5` above `.4` — reverse id
  order — precisely to avoid a crossing; that is crossing minimisation and must
  be stated, not copied as coordinates.
- **Skip edges: dummy nodes at intervening ranks**, routed in the inter-rank
  gutter. The draft's stub/gutter/stub routing only spans adjacent ranks, and
  longest-path ranking makes multi-rank edges inevitable.
- **Wires: per-segment rects with an interval union keyed by (axis, offset,
  colour class).** Required in both directions — the mock's shared gutter is
  one 57px rect at rest but splits into a dim 28px and a lit 29px run when
  traced, so edges cannot be merged per-edge; and naive emission double-composites
  `#ffffff38` where two edges share a gutter.
- **Colour: new named slots through the existing solver.** Flow needs wire,
  traced wire, dimmed wire, band fill, progress track, cursor fill, cursor
  keyline, chip fill, chip border, rank-label ink, and an agent hue distinct
  from `progress` — none exist in
  [[crates/scribe-client/src/beads_board.rs#BeadsBoardColors]]. Each is an
  alpha of an existing token or passes the same contrast lift the board already
  applies; the mock's hexes are the reference rendering under the mock's theme,
  and the *roles* are what the implementation reproduces.
- **Relative timestamps hand-rolled** from the ISO string `bd` already returns.
  No date dependency exists in `Cargo.toml`; adding one for `4h`/`38m` fails
  Principle 1.
- **Protocol: additive, gated, versioned.** New fields default via serde, a
  `Welcome` capability bit independent of `beads_detail` (matching the
  precedent that `beads_write` is independent of it), and a
  `REMOTE_PROTOCOL_VERSION` bump, per Principle 7.
- **Bounds and budgets (Principle 4, which the draft cited but never gave).**
  Graph capped at 200 nodes and 16 edges per node, matching
  `MAX_ITEMS_PER_QUEUE` / `MAX_BLOCKERS_PER_ITEM`; ranking and layout budgeted
  at under 2ms for a 200-node epic, measured by a unit benchmark; no added
  subprocess, so the graph read inherits the board's existing latency; per-frame
  node build stays within the 16ms budget by building only on-screen ranks.
- **Accessibility: nodes are buttons, edges are relationships.** Each node gets
  an AccessKit button role, a name of `"<id> <title>, <state>"`, a Tab stop,
  Enter/Space to move the cursor, and focus restored to the originating card on
  return to Lanes. Each node's blockers and dependents are announced in its
  description. No arrow traversal — that would breach the Non-Goal on keyboard
  graph navigation; ordinary Tab order along ranks is the contract.
- **Reduced motion changes nothing about the final frame** — Flow entry, cursor
  moves, and trace transitions are instant under reduced motion and land on the
  identical layout, matching the board's existing animation policy.
- **Visual assertions are sited on interior ranks.**
  `docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md`
  records this exact board's tooltip contract passing with the bug present
  because the assertion sat on a viewport-edge card where the clamp normalised
  both placements. Flow's node and wire probes go on rank 1 or 2, never the
  first or last, and the fixture needs a ≥5-rank fan-out/fan-in epic that does
  not exist today.
- **Geometry is derived, not transcribed.** Rank pitch is node width + gutter
  (262 + 24 = 286px); same-rank rows sit on a 56px pitch centred in the node
  band; wires attach at the node's **dot centre** (`top + 11`), not the node
  box's centre — the mock was itself wrong here until this review and has been
  corrected.

### Non-Blocking Observations

- Preserved-state lists disagree: Goals says pin, height, text scale; Story 5
  adds scroll position. Reconcile.
- Open Question 2 (drag targets in Flow) is already answered by Non-Goals ("no
  node dragging"); state it as a Non-Goal and close the question.
- The live node's sub line *replaces* the timestamp with the agent line in the
  mock, while the Normative reference lists id + separators + timestamp.
- The band differs between the mock's two frames — the trace frame drops the
  back control and the opened tag. "Contents in order" over-specifies.
- Backlog nodes and an `in_progress` node with no live session have no state
  treatment in either the mock or Story 3.
- Multi-blocker nodes have no defined sub-line text; the mock shows a single
  `waits on .6`.
- The chip's counts read transitive in the mock (`releases 3` for a node with
  one direct dependent) but direct in Story 4's wording.
- Flow nodes inherit no tooltip or click-to-copy, both of which Lanes cards
  have; retain or document the removal.
- `bd` version behind every measurement here is `1.1.0 (dev)`, matching the
  pinned image.
- Target Epic and Backlog Inputs will read as stale once the epic exists.

## Clarifications

**Q1: Does Flow replace the detail panel on card click, accompany it, or open from it?**

A: **Accompany.** The card always opens under the board exactly as it does
today — the panel is untouched — and the board swaps to the chosen card's
epic view. Flow changes what the strip shows, not what a click opens. This
retires Open Question 1, keeps
[[client#Client#Beads Board CLI Data Source#Guarded issue writes]] and the
whole panel test corpus intact, and removes the largest scope risk the review
found. Reflected in Story 1 and Story 2.

**Q2: Is the live-agent halo in v1, and can the Scribe Pi extension supply it?**

A: Yes to the extension, as its own slice. Verified against the installed Pi
docs: `pi.on("tool_call", …)` exposes `event.toolName` and a mutable
`event.input`, so the extension can observe a `bd … --claim` in a bash tool
call and knows `SCRIBE_SESSION_ID` from its environment — which is an *exact*
join, not a string match against assignee slugs.

The extension is the right observer but the wrong place to put beads knowledge
permanently, so the shape is: add one provider-neutral hook event
`issue_focused { issue_id }` to the existing channel schema, have
[[dist/pi-extension.ts#scribePiExtension]] emit it from `tool_call`, and let
the server bind issue → session in the live registry. Claude Code and Codex
adapters can emit the same event later without any transport change, exactly as
[[server#Server#Hook Channel#Adding a Provider]] intends. Scribe's side stays
provider-neutral.

Cost is real — a new `HookEventKind`, ingress mapping, a per-session issue
binding, the extension change, and harness coverage — so it is a **separate
task inside this epic, sequenced after the graph lands**. Story 3 already
requires Flow to degrade cleanly with no assignee data, so the halo can arrive
without reworking anything. The recorded parity gap is that a node assigned to
an agent Scribe cannot see stays in its ordinary state treatment: a missing
halo, never a false one.

**Q3: What does Flow do with a graph it cannot rank?**

A: Do not enter Flow. A card with no epic, or an epic that yields no graph,
leaves the board in Lanes and opens only the panel. No empty or degenerate Flow
frame is ever painted, so cycles, disconnected nodes, and external-blocker
placement stop being rendering problems and become one admission check.
Reflected in Story 1 and in Non-Goals.

**Q4: How does a graph that does not fit behave?**

A: Make the node as compact as it can be, then scroll horizontally. A vertical
scrollbar is to be avoided as far as possible, so the compact node budget exists
to keep ranks fitting the strip's height; the wheel over the graph scrolls it
horizontally. This makes node compactness a hard requirement rather than a
styling preference, and it obliges a revision of the normative mock — see
[Mock revision required](#mock-revision-required).

**Q5: Is the graph frozen at open, or live?**

A: Frozen at open. Clicking other nodes inside Flow opens them in the panel
**without the board switching** — the graph keeps showing the epic it was
opened on, and only a Lanes card click changes epics. Reflected in Story 2.

**Q6: Does Flow work in shared and remote windows?**

A: Local only for now. Flow rides the same gate as issue detail — local owner,
unshared window — so a remote or shared session simply never enters it and
keeps today's Lanes behaviour.

**Q7: Does the epic chevron do anything?**

A: Inert for now. Recorded in Non-Goals; it must not render as an interactive
affordance until it has an action.

## Mock revision required

Q4 makes the current mock non-normative in one respect: its node is 262×40px
with a 24px gutter, a 286px rank pitch, and a 56px row pitch. That budget fits
five ranks only across a full-width strip and only two rows per rank, which is
what forced the overflow question in the first place.

Before the mock can be treated as the pixel-perfect contract again it must be
revised for:

- the most compact node that still carries dot, priority, title, and id, with
  the sub line reduced or folded into the title line where it survives at the
  0.8–1.6 text-scale range;
- a rank pitch derived from that node rather than from 262px;
- three or more rows per rank inside the strip's node band;
- a horizontal scroll affordance and its edge treatment, with no vertical
  scrollbar in any state;
- the corrected wire anchoring already landed — edges attach at the node's dot
  centre (`top + 11`), not the node box's centre, which the first cut got wrong
  by 9px on every edge.

Until that revision lands, the Normative visual reference section describes the
*current* mock and is accurate only for the states it shows.

## Architecture Approach

Flow is a **second client-side rendering of data the server already reads**,
plus one typed request that hands the client an epic-scoped subgraph assembled
from the cached `bd list` response. No new `bd` invocation exists anywhere in
this feature.

The load-bearing finding is that
[[crates/scribe-server/src/beads_board.rs#load_board]] already runs
`bd list --all --limit 0 --skip-labels --sort created`, whose per-issue payload
carries `parent` and a typed `dependencies` array including **satisfied**
`blocks` edges. `classify_snapshot` discards both — it rebuilds `blocker_ids`
from the open-only `bd blocked` response and reduces `parent` to a display
name. Flow needs that discarded data, not a new read.

The subgraph cannot simply ride the existing snapshot, because
`MAX_ITEMS_PER_QUEUE` caps Done at 200 of 559 issues; an epic whose closed
members fall past the cap would render with holes. So the server keeps the
board caps for the board and answers a separate, epic-scoped, independently
bounded request from the same cached parse.

Alternatives rejected:

- **`bd dep tree <epic>` per open** — a second subprocess at the 5s
  `COMMAND_TIMEOUT`, plus a cache outside
  [[crates/scribe-server/src/beads_board.rs#BeadsBoardCache]]'s generation
  fence, which would leave Flow stale after every applied write. Rejected once
  the `list` payload was re-measured.
- **Client assembles the graph from board items** — defeated by the per-queue
  cap above.
- **Flow replaces the detail panel** — rejected at the clarify gate (Q1).
- **A general graph canvas** — the strip is 197px; a layered DAG with fixed
  ranks is the only layout that reads at that height.

**Admission is a server-side predicate, not a layout problem.** Q3 says Flow is
never entered without a graph, so the server refuses the epic outright when it
contains a cycle, a node disconnected from every other member, a blocker
outside the epic, or more members than the bound allows. The client then has
exactly two cases — a graph it can lay out, or Lanes — and the renderer carries
no degenerate-shape code at all. This supersedes the earlier "park cycle
members at `max_rank + 1`" decision from the spec review, which contradicted
Q3.

Layout is textbook layered-DAG over an already-admitted graph: longest-path
ranking, one barycenter pass for within-rank order, dummy nodes for edges
spanning more than one rank, and wires emitted as interval-unioned segments so
a traced sub-run can light independently and overlapping edges do not
double-composite. All of it is pure functions with no GPUI dependency,
which is what makes Story-level verification possible without a renderer.

Constitution: Principle 1 is honoured by keeping the graph read inside the
existing server-owned cache and adding no dependency (relative timestamps are
hand-rolled from the ISO strings `bd` already returns). Principle 2 is honoured
by reusing the panel unchanged and adding no new chrome idiom. Principle 5 and
6 are unaffected — no new egress, and the local-owner gate from Q6 narrows
rather than widens the trust boundary. Principle 7 requires the `lat.md` work
item below.

Learnings check:
`docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md` records
this exact board's tooltip contract passing while the bug was live, because the
probe sat on a viewport-edge card where the clamp normalised both placements.
Flow's visual probes are therefore sited on interior ranks, never the first or
last, and the fixture must contain a real fan-out/fan-in.

## Affected Components

- `crates/scribe-common/src/protocol.rs` — `BeadsBoardItem.parent_epic_id`;
  new `BeadsEpicGraph`, `BeadsGraphNode`, `BeadsGraphEdge`;
  `ClientMessage::RequestBeadsEpicGraph`; `ServerMessage::BeadsEpicGraph`;
  `Welcome.beads_flow`; `REMOTE_PROTOCOL_VERSION` bump.
- `crates/scribe-server/src/beads_board.rs` — retain `parent` id and typed
  `blocks` edges through the existing parse; assemble and bound an epic
  subgraph from the cached list; serve it under the current generation fence.
- `crates/scribe-server/src/ipc_server.rs` — admit the new request behind the
  same local-owner / unshared gate as
  [[crates/scribe-server/src/ipc_server.rs#beads_detail_connection_available]];
  advertise `beads_flow` in `Welcome`.
- `crates/scribe-client/src/beads_flow.rs` (new) — ranking, ordering, dummy
  nodes, wire segments, and the Flow renderer.
- `crates/scribe-client/src/beads_board.rs` — per-workspace mode state, the
  card click that opens the panel *and* swaps the strip, wheel routing, and the
  new colour slots on `BeadsBoardColors`.
- `crates/scribe-client/src/beads_panel.rs` — unchanged behaviour; gains only a
  retarget entry point for a Flow node click.
- `crates/scribe-client/src/main.rs` — dispatch the reply, latch the
  capability, route Esc and the wheel.
- `dist/pi-extension.ts` + `tests/e2e/func/pi-extension-harness.mjs` — the
  `issue_focused` emitter and its coverage.
- `.impeccable/mocks/beads-board-directions.html` — the compact-node revision
  Q4 forces.
- `tests/e2e/visual/beads-board.sh` — Flow probes on interior ranks.
- `lat.md/client.md`, `lat.md/protocol.md`, `lat.md/server.md`,
  `lat.md/test.md`.

## Data Model

No persistence and no migration. All new state is in-memory and dies with the
window.

Wire additions, all serde-defaulted so an older peer is unaffected:

- `BeadsBoardItem.parent_epic_id: Option<String>` — the id the display name is
  already derived from. This is what decides Flow eligibility client-side.
- `BeadsGraphNode { id, title, priority, status, queue, assignee: Option<String>, updated_at }`.
- `BeadsGraphEdge { from, to }` — `blocks` edges only; `parent-child` defines
  membership, not adjacency.
- `BeadsEpicGraph { epic_id, epic_title, closed, total, nodes, edges, truncated: bool }`.

Bounds mirror the board's: 200 nodes, 16 edges per node, `MAX_ID_CHARS` and
`MAX_TITLE_CHARS` truncation. **There is no partial graph.** An epic exceeding
the bound is refused like any other inadmissible shape, so `truncated` does not
exist and a cursor node can never be cut out of its own graph — the failure the
bound would otherwise have introduced. `assignee` and `updated_at` are new
fields on the server's issue parse, which reads neither today.

The reply is typed rather than an unexplained `Option`:
`BeadsEpicGraphOutcome::{ Graph(Box<BeadsEpicGraph>), NoGraph(reason), Unavailable(message) }`,
where `reason` distinguishes no-epic, cycle, disconnected, external blocker,
and too-large. The client renders none of them — every non-`Graph` outcome
means "stay in Lanes" — but the reason is logged server-side so an epic that
never opens is diagnosable.

Client state, per workspace:
`Option<FlowView { epic_id, cursor_issue_id, graph, layout, scroll_x }>` plus a
`pending: Option<(epic_id, generation)>` request fence. The fence is what stops
a late reply from reopening a graph after the user left Flow, clicked a second
card, or lost the capability on reconnect; a reply whose generation does not
match is discarded, exactly as the panel already discards a stale detail reply.
All of it is dropped on mode exit, workspace loss, `NotDetected`, capability
loss, and window close. Board polling continues while Flow is open and never
mutates `FlowView.graph` — the graph is frozen per Q5.

## API / Interface Changes

- `ClientMessage::RequestBeadsEpicGraph { workspace_id, epic_id }`.
- `ServerMessage::BeadsEpicGraph { workspace_id, epic_id, graph: Option<Box<BeadsEpicGraph>> }`
  — `None` distinguishes a vanished epic from a failed read, matching
  [[protocol#Protocol#Client Messages#Beads issue detail]]'s shape.
- `Welcome.beads_flow: bool`, defaulting false, independent of `beads_detail`
  and `beads_write` — the precedent
  [[protocol#Protocol#Server Messages#Connection#Beads write capability defaults safely]]
  sets.
- `HookEventKind::IssueFocused { issue_id }` on the hook channel, emitted by
  `scribe-hook-helper --provider=<id> --event=issue_focused --payload-stdin`.
- No breaking change: every addition is additive and serde-tolerant; an older
  client sees no `beads_flow` bit and never sends the request.

## Testing Strategy

- **Unit, renderer-independent** — ranking (longest path, multi-parent
  convergence, cycle parking), within-rank ordering (the mock's rank-2
  inversion must fall out of the barycenter pass, not be hardcoded), dummy-node
  insertion for skip edges, and wire segment interval-union including the
  shared-gutter split the trace state needs. These prove Goals 3 and Story 4
  without a window.
- **Protocol** — named MessagePack round trip for the new types; an old
  `Welcome` defaults `beads_flow` false; an old client receives no epic-graph
  frames.
- **Server** — epic assembly from a fixture `bd list` payload including a
  *closed* blocker edge (the case `bd blocked` cannot supply), the 200-node
  bound setting `truncated`, and the generation fence invalidating after an
  applied write.
- **Visual E2E** — a new fixture epic with a real fan-out/fan-in and at least
  five ranks. Probes sited on rank 1 or 2 only, per the recorded
  viewport-edge learning: wire endpoints land on dot centres, cursor treatment
  is unique, trace dims off-path nodes, and the band's tally matches the graph.
  Colour probes assert Flow's new slots derive from the theme, which today's
  contract does not cover for anything but ground and card fill.
- **Functional E2E against real `bd`** — click a card, assert the panel opens
  *and* the strip swaps; click a node, assert the panel retargets and the epic
  does not change; click an epic-less card, assert the strip stays in Lanes.
- **Pi extension harness** — `issue_focused` emitted on a bd claim seen through
  `tool_call`, and not emitted for unrelated commands.

Additional checks the alignment round found missing, each attached to its
owning work item rather than left to the final contract: horizontal wheel
clamping and the absence of a vertical scrollbar in every state; Esc precedence
when the panel holds focus; pin, height, and text-scale preservation across the
round trip; the cursor re-click no-op; an out-of-order retarget reply being
discarded; halo clearing on session end and isolation between windows; AccessKit
role, name, and activation on a node; reduced motion landing on the identical
final frame; `REMOTE_PROTOCOL_VERSION` mismatch and a reconnect with
`beads_flow=false` both leaving Lanes fully usable; and an epic member that the
capped Done lane omits still appearing in the graph.

Each user story maps to at least one independently reachable check, per
Principle 3.

## Risks

- **The mock is not yet compact enough to satisfy Q4.** Mitigation: the mock
  revision is a blocking work item ahead of the renderer, not a parallel one.
- **Rank/row budget may still not fit a real epic at text scale 1.6.**
  Mitigation: the layout engine is pure and unit-tested, so the fit is measured
  before any pixels exist, at both 0.8 and 1.6. The strip's reserved height is
  not a variable here — Story 5 preserves it and the 197px budget is a
  constraint — so the only lever is the compact node, and a rank that still
  overflows vertically is an admission failure like any other inadmissible
  shape rather than a scrollbar.
- **`issue_focused` is a schema change to a shipped channel.** Mitigation: it
  is additive, provider-neutral, and sequenced last; nothing else depends on it.
- **A wrong halo is worse than no halo.** Mitigation: the exact session-id join
  makes false positives structurally impossible; a missed claim degrades to no
  halo.
- **Board caps could still surprise.** Mitigation: `truncated` is on the wire
  and the band must show it rather than silently drawing a partial DAG.

## Sequencing

Shared primitives first. Three items are foundational because more than one
consumer needs them, and parallel workers branch from main and cannot see each
other's unlanded code: the protocol types, the layout engine, and the mock
revision. Two more were added by the alignment round because several items
would otherwise have raced on the same file.

**Foundational**

- **Protocol types, outcome enum, and capability** (P1) — `parent_epic_id`,
  the graph types, `BeadsEpicGraphOutcome`, `Welcome.beads_flow`, and the
  version bump. Blocks server assembly, layout, hook work, and render.
- **Mock revision for the compact node and horizontal scroll** (P1) — the
  normative contract; blocks the renderer and the visual contract. Must state
  the derived rank pitch (revised node width + revised gutter) rather than a
  copied constant.
- **Client layout engine** (P1, new file `beads_flow.rs`) — ranking, barycenter
  ordering, dummy nodes, wire interval-union, and the fit check at text scale
  0.8 and 1.6. Pure functions, no GPUI. Blocks every renderer-side item.
- **Flow colour slots on `BeadsBoardColors`** (P1, new) — extracted because the
  renderer, the trace item, and the halo item all add slots to the same struct
  and would otherwise diverge. Blocks render, trace, halo.
- **Multi-rank E2E fixture** (P1, new) — a seeded epic with a real fan-out and
  fan-in, at least five ranks, and a rank wide enough to exercise the row
  budget. Blocks both E2E items; no fixture existed for this shape.

**Server**

- **Retain parent id, typed `blocks` edges, assignee, and `updated_at` through
  the issue parse** (P1) — depends on protocol. The parse reads none of the
  last three today.
- **Epic assembly, admission predicate, and request dispatch** (P1) — depends
  on the parse item. Owns the cycle / disconnected / external-blocker /
  too-large refusals and the local-owner gate. Also owns the
  `ipc_server.rs` request arm.
- **Focused-issue session registry** (P1, new) — the `LiveSession` binding the
  hook event writes and the outbound liveness frame reads. Extracted because it
  and the request-dispatch item both touch `ipc_server.rs`.

**Client**

- **Flow renderer** (P1) — depends on layout engine, colour slots, mock
  revision.
- **Mode state, entry, exit, and wheel routing** (P1) — depends on renderer and
  epic assembly. Owns Story 1, Story 5, the Q3 no-graph path, the Q6 local-only
  gate, the request fence, horizontal scroll, and Esc precedence against panel
  focus. Merged with scroll rather than left unordered beside it: both own
  `beads_board.rs` and `main.rs` routing.
- **Node click retargets the panel** (P2) — depends on mode state. Story 2,
  including the cursor re-click no-op and overlapping-retarget reply discard.
- **Hover trace** (P2) — depends on renderer and colour slots. Story 4,
  including hover-over-cursor precedence, the chip's direct-edge counts and
  exact wording, one-frame restoration, and reduced-motion equivalence.

**Liveness (parallel to all rendering)**

- **`issue_focused` hook event, end to end** (P2) — depends on protocol and the
  focused-issue registry. Covers `HookEventKind`, the helper CLI selector,
  `hook_ingress` mapping, the registry write, the outbound liveness frame,
  clearing on session end and on `state_cleared`, and the Pi extension's
  `tool_call` emitter with harness coverage.
- **Live halo** (P2) — depends on the hook item, the renderer, and colour
  slots. Story 3's liveness half, including the negative case: assigned but not
  live renders no halo.

**Verification and documentation**

- **Visual contract for Flow** (P1) — depends on renderer, mode state, trace,
  and the fixture. Probes on interior ranks only. Asserts dot-centre wire
  anchoring, cursor uniqueness, each state's dot treatment, band composition,
  both wire classes under trace, no vertical scrollbar in any state, the inert
  chevron, and that every new colour slot derives from the theme.
- **Functional contract against real `bd`** (P1) — depends on mode state, node
  click, and the fixture. Card click opens the panel *and* swaps the strip; node
  click retargets without swapping the epic; an epic-less and a cycle-bearing
  card both stay in Lanes; two regions stay isolated; pin, height, and text
  scale survive the round trip; the existing drag and panel-write corpora still
  pass unchanged.
- **Layout benchmark** (P2) — depends on layout engine. Pins ranking plus
  layout under 2ms for a 200-node epic, the budget Principle 4 requires.
- **`lat.md` synchronisation** (P2) — depends on everything. Each owning bead
  updates its own sections as it lands and runs `lat check`; this item is the
  final reconciliation pass across `client.md`, `protocol.md`, `server.md`, and
  `test.md`, not the only place documentation happens.

**Epic**

All items are parented to a new `beads-flow-view` epic created at bead
materialisation, per Target Epic.

**Rollback.** No migration exists, so rollback is capability-shaped: clearing
`Welcome.beads_flow` stops the client entering Flow, any open `FlowView` is
dropped on the next reconnect, and Lanes remains the default rendering
throughout.

## Backlog Refinement

None. No P4 backlog inputs were supplied or discovered, so there is nothing to
refine, supersede, retire, or approve as a non-goal.


## Alignment fixes applied

One auto-fix round, two parallel passes (spec<->plan alignment, plan quality).
Must-fix findings applied directly:

- **Cycle handling contradicted Q3.** The architecture parked cycle members at
  `max_rank + 1` while the clarification says Flow is never entered without a
  graph. Resolved in favour of the clarification: cycles, disconnected nodes,
  external blockers, and over-bound epics are now one server-side admission
  predicate, and the renderer carries no degenerate-shape code.
- **Truncation could have cut the cursor out of its own graph.** Removed the
  `truncated` flag entirely; an over-bound epic is refused rather than served
  partial.
- **The reply was an unexplained `Option`.** Replaced with a typed
  `BeadsEpicGraphOutcome` carrying a refusal reason, so an epic that never opens
  is diagnosable.
- **No request fence existed.** Added `pending: (epic_id, generation)` so a late
  reply cannot reopen a graph after exit, a second click, or capability loss.
- **`issue_focused` was inbound-only.** Expanded to the full lifecycle:
  registry binding, outbound liveness frame, and clearing on session end.
- **Four sequencing races.** Mode state and horizontal scroll both owned
  `beads_board.rs` and `main.rs` — merged. Epic dispatch and the hook registry
  both owned `ipc_server.rs` — a focused-issue registry item was extracted.
  Renderer, trace, and halo all added slots to `BeadsBoardColors` — a colour-slot
  item was extracted. Both E2E items needed a fixture nobody owned — a fixture
  item was extracted.
- **E2E depended only on mode state** although its checks require node click and
  trace. Dependencies corrected and the two contracts separated.
- **Arrow traversal breached the keyboard Non-Goal.** Reduced to Tab plus
  Enter/Space.
- **The rank-overflow risk offered to raise the strip height**, contradicting
  the 197px constraint and Story 5's preserved height. Removed; overflow is an
  admission failure.
- **Target epic creation was absent from Sequencing.** Added.
- **Server parse gaps.** `assignee` and `updated_at` are read by neither the
  board parse nor the plan's original component list; both added.
- **Rollback, the layout benchmark, and per-bead `lat.md` ownership** were
  unstated; all three added.

Should-fix items accepted: the missing acceptance checks listed at the end of
Testing Strategy, and the derived rank-pitch formula folded into the mock
revision item.
