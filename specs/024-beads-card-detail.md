# beads-card-detail

## Problem Statement

The workspace Beads board shows five queues of cards but is read-only and
shallow: a card carries only id, title, priority, blockers, and epic, and the
only interactions are click-to-copy and scrolling. To read an issue's
description or acceptance criteria, edit any field, comment, claim, close, or
move an issue between queues, the user must leave the terminal for the `bd`
CLI — losing exactly the context the board exists to keep in view.

This feature makes the board a full working surface: clicking a card opens an
anchored detail panel — rendered pixel-perfect to the approved mockup
`.impeccable/mocks/beads-card-detail.html` — where every bd-editable field can
be viewed and edited with correct persistence through bd, and cards can be
dragged directly between queues with an extremely smooth, non-laggy drag.

## Goals

- Clicking a card on a hovered or pinned board opens the detail panel for that
  issue, anchored under its lane, populated with the issue's full field set
  (description, acceptance criteria, notes, design, spec, labels, people,
  dates, dependencies, comments) — data the board snapshot does not carry
  today, so a new server request supplies it.
- The panel is judged pixel-perfect against
  `.impeccable/mocks/beads-card-detail.html` by the Docker visual e2e suite,
  the same way `tests/e2e/visual/beads-board.sh` judges the board against
  `.impeccable/mocks/beads-compact-live-overview.html`.
- Every bd-editable field listed in Constraints round-trips: edit in panel →
  server runs the bd verb → board snapshot refreshes → panel and board repaint
  the persisted value. Verified end-to-end in the functional harness against a
  real bd project fixture.
- A failed write never destroys state: the panel keeps last-good values,
  surfaces the failure non-destructively, and the board keeps its last-good
  snapshot (same posture as the existing refresh path).
- Dragging a card to another lane moves the issue between queues via the
  matching bd verb. The drag is smooth by construction and by measurement: the
  ghost follows the pointer with at most one frame of latency, no synchronous
  bd or IPC I/O happens on the render path, the write runs off-thread on drop
  with optimistic placement and revert on failure. The visual suite drives a
  scripted drag and requires the ghost to track the synthetic pointer
  trajectory and the board to stay frame-stable (no stall, no perturbed
  terminal rows below).
- A plain click still opens the panel: drag engages only past GPUI's ~2px
  drag-arm threshold, mirroring the titlebar's click-swallowed handling.
- All pointer gestures on the board are consumed by the board's press chain
  (as `press_board_edge` consumes the resize grip press) so a mouse-reporting
  terminal application below never sees them.

## Non-Goals

- Creating new issues from the board or panel.
- Editing the dependency graph (adding/removing blocks links) or reparenting;
  the panel shows the graph, it does not rewire it.
- Reordering cards within a lane — queues have no user-defined order.
- Dragging cards between regions/workspaces.
- Comment editing or deletion (bd's thread is append-only).
- Dolt sync operations (`bd dolt push`/`pull`) or any git-side effects.
- Editing sparse lifecycle fields (due, defer date, estimate, external ref,
  metadata) — they render when set, but phase-one editing is limited to the
  field list in Constraints. (Open question 2 records the defer exception.)
- Reopening closed issues from the panel — a Done issue's panel shows no
  write verbs. (The transient post-close undo is not reopening: it is a
  guarded status-open within the undo window only.)
- Keyboard-driven panel operation: the panel is pointer-driven except Esc;
  key routing while editing IS specified (keystrokes never reach the PTY),
  but keyboard navigation/activation of fields is follow-up work.
- Opening an issue without a visible card: no open-by-id, no board
  search/filter; long-closed issues fall off the Done lane and are
  CLI-territory.
- Bundling a Beads reader or touching Dolt directly — everything continues to
  go through the installed `bd` CLI per the existing server contract.

## Backlog Inputs

None. No `source_backlog` or epic was supplied, and no open P4 issues in the
tracker describe this feature.

## Target Epic

Resolved: no existing epic covers this feature; this run creates a new epic
("Beads board card detail panel and drag-to-queue") at bead creation.

## User Stories

### Story 1 — Read an issue in full from its card

As a developer working in Scribe, I want to click a board card and read the
whole issue — description, acceptance criteria, notes, design, spec, labels,
people, dates, dependents, comments — so that I never have to leave the
terminal to know what a bead actually says.

Acceptance criteria:
- Clicking a card (press + release within GPUI's drag-arm threshold) on a
  hovered or pinned board opens the detail panel for that issue, anchored
  under the clicked card's lane and clamped to the region.
- The panel renders the full anatomy of the approved mock: full-bleed head
  (bare heat-ink priority mark, title, epic in its hue, close ×; identity row
  with id/type/labels/filed-by left and SPEC/DESIGN pointers right; state row
  with derived queue word in queue colour, derivation phrase, and
  assignee/date facts right), dependency-thread spine with queue-coloured
  haloed node and coral UNBLOCKS junction, typeset body (description lead,
  run-in ACCEPTANCE and NOTES heads), newest-first comment thread, footer
  status rail with claim / close issue verbs.
- Field data comes from a new workspace-scoped detail request (the board
  snapshot's truncated items are not the source); opening shows the panel
  immediately with a loading state if the detail fetch is still in flight.
- Empty fields render no row. A closed issue's close reason and date appear in
  the state row. Blockers, when present, appear as upstream nodes on the
  thread above the issue's own node.
- The ID click-copies in full through the existing copy surface. Dependent
  chips on the UNBLOCKS line navigate the open panel to that issue.
- Esc, ×, or a click outside closes the panel. Long content scrolls inside a
  max height of ~70% of the region.
- The panel is keyed per workspace like hover/pin/snapshot state; two regions
  can each show their own panel without interference.

### Story 2 — Edit any field and trust it persisted

As a developer, I want to edit issue fields directly in the panel and see the
persisted result, so that quick corrections do not require the CLI.

Acceptance criteria:
- Every passage is click-to-edit in place using the settings window's
  single-inline-editor pattern: title, description, acceptance criteria,
  notes, design, spec-id, labels.
- Clicking the priority mark unfolds the P0–P4 pick row in place; picking
  writes the priority.
- The status rail writes exactly `open`, `in_progress`, `closed`; Ready,
  Blocked, and Backlog remain derived and are presented as such.
- The status rail's three words are click targets writing exactly their
  status (visually as mocked — words breaking the rail, no button chrome).
- Claim runs the claim verb (assignee = actor, status in_progress); close
  issue closes with no reason prompt, then shows a transient
  "closed <id> · undo" affordance for a few seconds whose undo writes
  status open (guarded).
- "add a comment…" posts a comment; the thread shows it after refresh.
- A committed edit runs the bd verb server-side, then refreshes the board
  snapshot; the panel repaints from persisted data, not from the local edit
  buffer. Functional e2e proves persistence by re-running `bd show` inside
  the harness fixture.
- A failed write (bd exit non-zero, timeout, envelope mismatch) leaves the
  previous value in place and surfaces the failure without destroying the
  edit target or the board.
- Concurrent-writer safety: claim, status, close, and undo writes carry
  `--if-assignee` / `--if-status` guards captured from the fresh detail
  read, so a race with another actor (this repo's own agents claim beads
  concurrently) fails cleanly as a surfaced "someone else won" notice;
  text-field edits are last-writer-wins in v1. The `beads_write` capability
  stays off on bd 1.1.0 because that release lacks both guard flags.

### Story 3 — Move a card between queues by dragging

As a developer triaging work, I want to drag a card from one lane to another,
so that status changes are one gesture instead of a CLI round trip.

Acceptance criteria:
- Pressing a card and moving past the drag-arm threshold lifts a drag ghost
  of the card that follows the pointer with at most one frame of latency;
  releasing within the threshold still counts as the click that opens the
  panel.
- Drag sources: only Backlog, Ready, and In-progress cards can be lifted;
  Done and Blocked cards are not drag sources (reopening and unblocking are
  not drag semantics).
- Lane targets map to verbs per the server's classification precedence
  (`classify_snapshot` in `crates/scribe-server/src/beads_board.rs`):
  Done = close (then the same transient undo as the panel's close verb);
  In progress = claim (guarded — a race with another actor's claim fails
  cleanly); Ready = status `open` with any defer date cleared (the
  classifier's next snapshot is authoritative if the issue still fails
  `bd ready`); Backlog is not a drop target — bd has no clean
  "make it backlog" verb — and rejects like Blocked.
- Blocked and Backlog are rejected drop targets: a no-drop presentation
  while hovering (the lane's wash dims within the board's colour
  discipline) and snap-back on release, with no write issued.
- On a legal drop the card optimistically appears in the target lane while
  the write runs off-thread; on failure the card reverts to its source lane
  and the failure surfaces non-destructively.
- No synchronous bd or IPC I/O occurs on the render path during a drag; the
  per-frame drag update is O(1) state mutation plus paint.
- The whole gesture is consumed by the board press chain: a mouse-reporting
  application in the pane below receives none of the press/move/release, and
  starting a drag on a hover-opened board holds the board open for the
  gesture's duration (as the resize-grip drag already does).
- The visual suite scripts a drag from Ready to In progress and asserts the
  ghost sits within 3px of the synthetic pointer at each `--sync` waypoint,
  the card lands in the target lane, and the terminal rows below repaint
  unperturbed (frame-stability check); the functional suite proves the
  drop's verb landed in real bd.

### Story 4 — See the graph and act on state truthfully

As a developer, I want the panel's state and graph claims to reflect bd's
actual semantics, so that what I read and drag matches what persists.

Acceptance criteria:
- The state row's queue word matches the server classification for the issue
  at snapshot time; the derivation phrase matches (e.g. Ready shows upstream
  clear; Blocked shows its blockers as upstream thread nodes).
- The UNBLOCKS junction lists the issue's dependents (issues whose
  dependencies include this one), each clickable-through; the count and set
  come from bd data, not inference.
- Status writes that produce derived-queue changes land where the classifier
  puts them (e.g. status `open` on an issue with open blockers lands in
  Blocked, and the UI communicates that rather than pretending the drop
  target won).
- All panel colours derive from the live theme exactly as the board's do
  (chrome slots + ANSI queue hues + solved priority tints, contrast-lifted
  per the board's floors); an opacity or theme edit rebuilds the panel
  palette. No hardcoded palette values.

## Constraints

- Visual source of truth: `.impeccable/mocks/beads-card-detail.html`
  (user-approved 2026-08-14). Pixel-perfect target; the spec, implementation,
  and visual e2e reference this file directly, the same way the board
  references `.impeccable/mocks/beads-compact-live-overview.html`. Board
  cards keep their filled priority badge; the panel uses the bare heat-ink
  mark (both from the same solved heat scale).
- All bd writes go through the server, which owns the bd subprocess contract:
  absolute executable resolution, `-C` plus cwd in the project root,
  versioned JSON envelope pinned to schema 1, five-second deadline,
  process-group cleanup, direct argv. The current contract is read-only
  (`--readonly` posture); a deliberate, minimal read-write verb set must be
  added without weakening the read path: update (title, description,
  acceptance, notes, design, spec-id, priority, type, labels via
  add/remove/set, status with an optional defer-clear), claim, close
  (reasonless; undo is a guarded status open), comment add, and the uncached
  detail read (`bd show --json --include-comments`). Assignee changes happen
  only through claim and undo. The functional suite exercises writes against
  real bd; the checked-in fake bd scripts remain server unit fixtures only.
- Every successful write triggers a board snapshot refresh for that root so
  client repaints persisted state; failure preserves last-good state
  server-side and panel-side (mirrors the existing cache posture).
- Client architecture constraints: the panel paints inside its workspace's
  region like both board modes (never window-wide); hover/pin/snapshot/panel
  state stays keyed per workspace; the board's free-function build pattern
  (no window-handle reach; parked copy/intent state lifted by the view)
  extends to panel intents (edits, verbs, navigation); the existing press
  chain (`TerminalView::press_board_edge` and siblings) grows the card
  press/drag/click resolution.
- The board's per-window text-size stepping and per-workspace board height
  interact with the panel: the panel adopts the board's text scale, and its
  anchor/clamp math must hold at all board heights and text scales.
- Performance budget (constitution 4): drag ghost latency ≤ 1 frame behind
  the pointer; no synchronous subprocess/IPC on the render path during drag
  or panel open; snapshot refresh after a write stays off-thread. Named
  verification: the visual-suite scripted drag (ghost-tracking +
  frame-stability assertions) plus the functional suite's persistence checks;
  code-level, the drag update path is reviewable as O(1) per frame.
- Validation runs ONLY in the Docker e2e harness (`just e2e-func` /
  `just e2e-visual`, `--network none`), never against the host install. The
  functional image's checked-in fake `bd` fixtures grow write-verb coverage;
  the real-bd refresh test may grow a real write round-trip.
- Typed protocol additions (constitution 1): new request/response messages
  for issue detail and issue writes are named MessagePack messages in
  `crates/scribe-common/src/protocol.rs` with round-trip tests, not a generic
  passthrough that would let the client compose bd argv.
- Trust boundary (constitution 5): writes originate only from direct user
  gestures in chrome (panel/board), never from PTY content; bd argv is
  composed server-side from typed fields with direct argv (no shell); the
  actor recorded on writes is the local user identity bd already resolves.
- `lat.md` stays synchronized (constitution 7): the Beads Board CLI Data
  Source section and test specs grow the panel, write path, and drag
  behavior; `lat check` passes.

## Open Questions

1. **Backlog vs Ready drop verb.** Classification: Ready = membership in
   `bd ready` output; Backlog = open issues not in it (deferred issues and
   whatever else `bd ready` excludes). What exact verb should a drop into
   Backlog run — `bd update --defer <date>` (park; then what date), priority
   P4 (bd's "backlog" priority convention), or should Backlog reject drops
   like Blocked? Needs a decision; affects both drag and the status rail copy.
2. **Defer editing.** If Backlog-drop uses defer, does the panel also need to
   surface/edit the defer date (currently in the render-when-set, no-edit
   bucket)?
3. **In-progress drop: claim or bare status?** Dropping into In progress can
   run `--claim` (assigns to the actor) or bare `--status in_progress`
   (leaves assignee untouched). Which default? Claim matches bd's model of
   in-progress work but self-assigns on behalf of the user.
4. **Close reason UX.** `bd close --reason` is optional. Does the close verb
   prompt inline for a reason (extra step on every close) or close
   immediately with no reason (faster, loses information)? Mock shows no
   prompt.
5. **Concurrency guards.** Should every panel write carry `--if-status` /
   `--if-assignee` guards captured at panel-open time (fails cleanly on
   races, but may annoy on stale panels), or only the claim/status verbs?
6. **Detail freshness.** Does an open panel poll (like the pinned board's
   60s) or fetch once on open with refresh only after writes? Does the
   detail read share the 30s snapshot cache or bypass it?
7. **Actor identity.** bd resolves actor from git user.name/$USER on the
   server host. Comments and edits will be attributed to that identity — is
   that acceptable, or should Scribe pass an explicit `--actor`?
8. **Closed-issue panel.** Which verbs remain on a Done card's panel —
   nothing, or reopen (status open)? Reopen is currently a Non-Goal;
   confirm.
9. **Type editing surface.** Type (`feature`/`task`/`bug`/…) is in the verb
   set but the mock shows it as plain text in the identity row — is it a
   pick row like priority, or inline text with validation?
10. **Blocked-drop affordance detail.** Reject-on-release with snap-back is
    specified; is a hover-time signal (lane dim / no-drop cursor) required
    too, and what does it look like within the board's colour discipline?
11. **Drag source restrictions.** May Done cards be dragged out (implies
    reopen semantics, currently a Non-Goal)? May Blocked cards be dragged
    (their blockers persist; only closed/in_progress/open writes apply)?
12. **Panel width at narrow regions.** The mock's 560px panel needs a rule
    for regions narrower than the panel plus margins (clamp to region width?
    minimum readable width?).
13. **Comment length bounds.** `--include-comments` is unbounded and "may be
    slow on issues with many comments" per bd help — does the detail request
    cap comment count/bytes like the snapshot caps items?

## Clarifications

Answered by the owner at the clarify gate (2026-08-15): all recommended
options accepted, no technical decisions vetoed.

**Q1: Full edit coverage vs trimmed v1 edit set?**
A: Full coverage — title, description, acceptance criteria, notes, design,
spec-id, labels, priority, type, status/claim/close/undo, comments. Type
edits via a pick row over bd's enumerated types, like priority.

**Q2: Are the status rail words click targets?**
A: Yes — the three words write their status while keeping the mock's
appearance (words breaking the rail, no button chrome).

**Q3: Drag verb mappings?**
A: In-progress drop = claim (guarded). Backlog is not a drop target
(rejected like Blocked; bd has no clean parking verb). Drag sources are
Backlog/Ready/In-progress cards only; Done and Blocked cards cannot be
lifted. Ready drop = status open + defer cleared; the classifier's next
snapshot is authoritative.

**Q4: Close friction and misdrop recovery?**
A: Reasonless close everywhere (footer verb and drag-to-Done), followed by a
transient "closed <id> · undo" affordance for a few seconds; undo writes
status open with guards. No confirm dialog.

**Q5: Open animation?**
A: In v1 — a short (~120ms) card-lift/scale animation from the clicked card
to the panel; the animation's end state must satisfy the visual assertions.

**Open-question resolutions** (gate answers plus unvetoed technical
decisions): OQ1 Backlog rejected as drop target, Ready drop clears defer;
OQ2 no defer editing in v1 (only the implicit clear on a Ready drop); OQ3
claim; OQ4 reasonless + transient undo; OQ5 guards on claim/status/close/
undo only; OQ6 fetch on open + refresh after own writes, no poll; OQ7 host
identity, no --actor; OQ8 Done panels show no write verbs; OQ9 type is a
pick row, in scope; OQ10 no-drop = lane wash dims + snap-back; OQ11 Done and
Blocked cards are not drag sources; OQ12 the panel clamps to the region
width minus margins with a 400px floor — narrower regions get no panel —
and visual assertions pin text scale 1.0 at full width; OQ13 the server caps
the thread at the newest 50 comments with per-field byte caps and a visible
hidden count.

## Spec Review

Six parallel review passes (requirements, gaps, ambiguity, feasibility,
scope, stakeholders) against the draft and the constitution. Cross-dimension
hits were merged; product decisions go to the clarify gate, technical
questions were resolved by code investigation and are recorded below for
veto.

### Critical Questions (answer before planning)

1. Full edit coverage vs a trimmed v1 edit set — the owner asked for "all
   fields", and the review confirms the inline editor is new client
   machinery either way, but labels/design/spec-id/type editing add surface
   for verbs the CLI serves rarely; confirm full coverage or name the trim.
   Flagged by: scope, feasibility.
2. Status rail writability — the spec says the rail writes
   open/in_progress/closed but the approved mock renders the status words
   inert (only claim/close issue are buttons); two engineers build different
   panels. Flagged by: ambiguity.
3. Drag verb sub-mappings — drag itself is owner-requested and stays, but
   three mappings are undefined: In-progress drop (claim vs bare status),
   Backlog drop (no clean bd verb exists — reject like Blocked, or defer),
   and drag sources (may Done/Blocked cards be lifted). A Backlog→Ready drop
   is a no-op as drafted. Flagged by: requirements, scope, stakeholders,
   ambiguity.
4. Close friction and misdrop recovery — close is one unconfirmed gesture
   (drag-to-Done especially) while reopen is a Non-Goal, so a misdrop is
   unrecoverable from the UI; choose reasonless-close + transient undo,
   confirm-on-close, or documented CLI-only recovery. Flagged by: gaps,
   stakeholders, requirements.
5. Open animation — the mock's annotations promise a card-grows-into-panel
   open animation the spec never adopts; in v1 or Non-Goal? Flagged by:
   ambiguity.

### Technical Decisions (self-resolved — veto at the gate to override)

- "Pixel-perfect" operationalized per the board precedent: structure, sizes,
  and weights from the mock, colours from the live theme, verified by an
  enumerated sampled-assertion inventory at text scale 1.0 (spine node +
  halo, run-in heads, rail break and stop-short gap, empty-field row
  omission, epic hue, priority ink, comment clamp) — not image diffing.
- New typed protocol messages (issue detail request/response, typed write
  verbs) gated by a Hello/Welcome capability bit (precedent: `ci_run_bar`);
  an old server leaves the board read-only with no wedged panel; the
  compatibility decision is documented per constitution 7.
- Writes are accepted only from local, owning controller connections whose
  window shows that root (precedent: `DismissCiRun` gating); shared/remote
  viewers get a read-only panel. With writes local-only, the bd actor is the
  host identity and needs no `--actor` override.
- Write/refresh ordering: writes serialize per canonical root behind a
  generation fence; a refresh that started before the last committed write
  is discarded; a successful write forces an immediate refresh pushed to
  every board on that root, and an open panel re-fetches detail.
- Writes get their own deadline (15s, vs 5s reads) because bd writes hit
  Dolt commit + JSONL export; on timeout the client converges via forced
  refresh + detail re-read instead of trusting the failure. The bd 1.1.0
  probe's slowest of 20 timed write attempts was 972ms.
- Concurrency guards on claim/status/close require `--if-status` and
  `--if-assignee` captured from the fresh detail read. bd 1.1.0 has neither
  flag: each is rejected as an unknown flag with exit 1, not exit 13. The
  server must classify that contract as write-incompatible, not as a race;
  text-field edits remain last-writer-wins once writes are enabled.
- bd 1.1.0 is the detail-read floor, not the write floor. The write floor is
  the first harness-verified release whose update command exposes both guard
  flags and whose mismatch exit contract is measured. Until then the panel
  stays read-only instead of silently dropping guards.
- The inline editor is new client machinery, the feature's largest work
  item: a focusable editor entity implementing `EntityInputHandler` with a
  key-routing carve-out so keystrokes never reach the PTY. Esc precedence:
  cancel edit → close panel → terminal. Commit matrix: Enter commits
  one-line fields; blur or clicking another passage commits; Esc cancels;
  multi-line fields commit on blur or modifier-Enter. Editing is
  window-exclusive.
- Drag uses GPUI's native drag-and-drop as the titlebar does (`on_drag`
  ghost entity, `has_active_drag` click-swallow): the 2px arm, click
  swallowing, and a window-layer ghost that escapes lane clipping come for
  free; the press chain's only growth is gating PTY mouse forwarding during
  a drag.
- Ghost budget verified stepwise in the visual suite (xdotool move `--sync`
  → screenshot → ghost within N px of the pointer at each waypoint;
  terminal rows below unperturbed); the per-frame drag update is reviewed
  O(1) with no synchronous I/O.
- Persistence proof is mandatory in the functional suite against real bd
  (write → `bd show` round-trip for at least one verb per family); the
  visual image has no bd and injects snapshots, so it carries geometry/ghost
  assertions only.
- Detail read is
  `bd show --json --include-comments --include-dependents`: default output
  has `dependent_count` but no dependent ids, so the second flag is required.
  The server applies caps (latest N comments, per-field byte caps) and a
  visible hidden-count when truncated; detail supersedes snapshot
  truncation.
- Panel data: fetch on open, refresh after own writes, no poll. Loading
  state renders the head from card data over a placeholder body and is
  exempt from pixel assertions. The panel tracks its issue by id, re-anchors
  on lane change, and closes with a notice on 404/NotDetected.
- Failure surface: a one-line coral-inked notice inside the panel (drag
  failures revert the card with the same notice on the board), auto-clearing
  on the next success; every write attempt logs one structured server line
  (verb, issue, outcome, bd error detail).
- Client-side write deadline and reconnect reconciliation: optimistic state
  reconciles against the first post-reconnect snapshot.
- The server write-verb surface is trimmed to exactly what the shipped UI
  invokes (constitution 5).
- Mock authority: spec text governs behavior, the mock governs appearance.
  Annotation behaviors adopted: comments expand in place, the ID shows its
  copy glyph on hover. The open animation is gate question 5.
- Non-Goals additions: keyboard-driven panel operation (Esc only, key
  routing still specified), open-by-id/board search, reopen from the panel
  (a Done issue's panel shows no write verbs).

### bd 1.1.0 contract probe

The 2026-08-15 Docker functional probe measured the pinned CLI without
touching the host tracker and found one write-blocking contract mismatch.

`bd update --help` lists neither `--if-status` nor `--if-assignee`.
Mismatched attempts with each flag returned exit 1 and `unknown flag`, not
exit 13. The issue JSON before and after both attempts was identical, so bd
rejects rather than ignores them, but 1.1.0 cannot provide the required
guarded writes or the planned precondition-failed mapping.

Default `bd show --json` returns `dependent_count` and `comment_count` but
neither collection. `--include-dependents` adds `dependents`, whose elements
contain `id`, `title`, `status`, `priority`, `issue_type`, `created_at`,
`updated_at`, and `dependency_type`. Dependent-chip ids therefore require
that flag.

`bd comments add --json` returns one object with `author`, nanosecond RFC3339
`created_at`, `id`, `issue_id`, `schema_version: 1`, and `text`.
`bd comments <id> --json` returns an array whose elements have `id`,
`issue_id`, `author`, `text`, and second-resolution `created_at`.
`bd show --json --include-comments` embeds the same element shape in the
issue's `comments` array; observed order was oldest first.

The 20 timed write attempts took, in order, 504, 517, 490, 480, 477, 459,
972, 622, 459, 485, 558, 513, 448, 421, 483, 429, 663, 582, 67, and 68ms.
The first 18 succeeded across field, label, status, comment, claim, close,
and reopen verbs. The last two were the rejected guard attempts. All were
below 15,000ms; maximum was 972ms.

### Non-Blocking Observations

- The feature decomposes into independently shippable slices with distinct
  risk: (a) detail read + read-only panel, (b) write verbs + editing, (c)
  drag — beads should sequence a→b→c even with all three in scope.
- Day-after asks to expect: open-by-id/search, keyboard navigation,
  dependency editing, issue creation — all recorded as Non-Goals.
- Scale/latency bounds: no panel-open latency target exists beyond the bd
  deadline; anchor/clamp behavior should be tested at named sample points
  (min/max board height, 0.8×/1.6× text scale, narrowest region) rather
  than "all".
- Agent-authored comment threads are the long ones; the comment cap must
  surface truncation, never silently drop the tail.
- Dependent-chip navigation stays (approved mock behavior) but is the first
  candidate to simplify if the read slice needs to shrink.

## Architecture Approach

Three independently shippable slices on one epic, sequenced by risk: (a) a
typed detail read plus the read-only panel, (b) the write verb set plus
inline editing, (c) drag-to-queue. The server remains the only process that
touches bd (constitution 1, 5): the client sends typed intents and paints
typed results, and bd argv is composed server-side from typed fields.

Slice a ships the mock's full anatomy with every write affordance rendered
but inert — exactly the presentation a shared viewer or an old server gets
permanently — so the visual contract is the full inventory from slice a and
interactivity is what slice b adds. The capability surface splits
accordingly: `beads_detail` (slice a) gates the panel, `beads_write`
(slice b) arms its verbs; a server is never in a state where it advertises
writes it cannot handle. Client-side work parallelizes against protocol
types plus injected fixtures rather than serializing behind server work.

The panel is a new per-window GPUI entity (`BeadsDetailPanel`) rather than
an extension of the board's free-function build: the board build stays a
free function with parked intents, but editing needs focus, an
`EntityInputHandler`, and key interception — capabilities only an entity
carries (the GPUI overlays and settings window set this precedent). The
panel entity is window-exclusive for editing while its open/anchor state
stays keyed per workspace, so two regions can each show a panel but only one
edit is armed at a time.

The editor spike verified the main-window routing constraint against the
pinned GPUI revision. `Window::handle_input` must re-register the focused
editor's `ElementInputHandler` during every paint. While that focus is armed,
the terminal root must return before both its unconditional
`stop_propagation` and PTY encoder: an un-stopped printable `KeyDown` is what
makes GPUI call `replace_text_in_range`. Enter arrives there as `"\n"`;
Escape and modified controls remain key-only and belong to the panel's key
listener. A headless window test sends printable, Enter, Escape, and Ctrl-C
through this branch and observes no call to the real terminal encoder, then
restores terminal focus and observes normal encoding.

Drag rides GPUI's native drag-and-drop exactly as the titlebar does
(`on_drag` ghost entity, `has_active_drag` click-swallow): the ~2px arm,
click swallowing, and a window-layer ghost that escapes lane clipping come
from the framework; the press chain's only growth is gating PTY mouse
forwarding while a drag is active. Alternatives rejected: a modal dialog
surface (the anchored panel is the approved design), client-side bd
invocation (trust boundary), a generic bd-passthrough message (would let
the client compose argv; constitution 1 and 5), and extending the
window-status-bar press chain with card rects (element listeners plus
native DnD are cheaper and already precedented).

## Affected Components

- `crates/scribe-common/src/protocol.rs` — new typed messages (detail
  request/response, write request/result), a `beads_write` capability bit on
  Hello/Welcome (precedent: `ci_run_bar`), round-trip tests.
  `BEADS_BOARD_PROTOCOL_VERSION` stays 1: the additions are new named
  messages, not changes to the snapshot payload; documented per
  constitution 7.
- `crates/scribe-server/src/beads_board.rs` — detail fetch
  (`bd show --json --include-comments --include-dependents`), server-side
  caps, the write verb set with guards, a separate 15s write deadline,
  mismatch-exit mapping verified against the eventual write-floor release,
  per-root write generation fence, post-write forced refresh, and a bd
  contract probe that keeps writes unavailable when either guard flag is
  absent. bd 1.1.0 remains sufficient for detail reads only.
- `crates/scribe-server/src/ipc_server.rs` — handlers for detail and write,
  gated to local owning controller connections whose window shows the root
  (precedent: `DismissCiRun`), post-write snapshot push to every board on
  that root, one structured log line per write attempt.
- `crates/scribe-client/src/beads_board.rs` — card press/click resolution,
  selected-card state, drag source wiring and no-drop lane dimming,
  optimistic overlay and revert, board-side transient notices (undo,
  failure).
- New `crates/scribe-client/src/beads_panel.rs` — the panel entity: layout
  per the mock, theme-derived palette with the board's contrast lifting,
  anchor/clamp math against `PaneShell::board_rect` with the 70%-of-region
  max height and internal scroll, re-anchoring when the issue changes lanes,
  the 120ms open animation, ID click-copy and dependent-chip navigation
  intents, comments expanding in place with the hover copy glyph, the
  read-only (verbs-inert) presentation for closed issues and non-owning
  viewers, and the inline editor (`EntityInputHandler`) with the commit
  matrix and key carve-out.
- `crates/scribe-client/src/main.rs` — per-workspace panel state beside
  hover/pin/snapshot, PTY mouse-forwarding gate during drag, hover-board
  hold-open while a drag or open panel is active, client-side write deadline
  and reconnect reconciliation of optimistic state, panel entity wiring and
  dismissal routing (Esc precedence).
- `docker/Dockerfile.func` and `tests/e2e/` — a writable bd project fixture,
  functional persistence/drag suites, visual panel-contract and
  ghost-tracking suites.
- `lat.md/` — client, protocol, server, and test sections.

## Data Model

No persistent storage changes and no migrations. New wire/in-memory types:

- `BeadsIssueDetail`: id, title, description, acceptance, notes, design,
  spec id, status, priority, type, labels, assignee, owner/filed-by,
  created/updated/closed timestamps, close reason, defer/due/estimate/
  external-ref when set, blockers (id+title), dependents (id+title),
  comments (author, timestamp, body) capped to the newest 50 with a
  hidden-count, per-field byte caps, and the server-derived queue plus its
  derivation basis (the same `classify_snapshot` precedence), so the state
  row never re-derives client-side.
- `BeadsIssueWrite` verb enum: SetTitle, SetDescription, SetAcceptance,
  SetNotes, SetDesign, SetSpecId, SetPriority, SetType, SetLabels,
  SetStatus(open|in_progress|closed, with a clear-defer flag on open so a
  Ready drop parks nothing), Claim, CloseIssue, UndoClose, AddComment — each
  carrying optional `if_status`/`if_assignee` guards captured from the
  fresh (uncached) detail read.
- `BeadsIssueWriteResult`: Applied { generation } |
  PreconditionFailed | Failed { reason }.
- Client: per-workspace panel state (open issue id, detail, loading/error,
  undo window), window-exclusive edit state, drag state (source card,
  target lane, optimistic overlay tagged with the write generation).
- Server: per-canonical-root write generation counter; refreshes started
  before the latest committed write are discarded on completion.

## API / Interface Changes

- `ClientMessage`: `RequestBeadsIssueDetail { workspace_id, issue_id }`,
  `BeadsIssueWrite { workspace_id, issue_id, verb, guards }`.
- `ServerMessage`: `BeadsIssueDetail { workspace_id, detail | not_found }`,
  `BeadsIssueWriteResult { workspace_id, issue_id, result }`; the existing
  board snapshot message is reused for post-write pushes.
- `Welcome` advertises `beads_detail` and `beads_write` separately; a client
  on an old server shows today's read-only board, and a client on a
  detail-only server shows the panel with inert verbs — never a wedged
  panel, never an armed editor the server cannot serve. No breaking
  changes; all additions are new named MessagePack messages with round-trip
  tests.
- UI surfaces: click-to-open panel; inline editors; priority and type pick
  rows; writable status rail; claim/close/undo; comment composer; drag with
  ghost, no-drop affordances, optimistic placement.

## Testing Strategy

- Server unit: verb→argv composition including guards, labels, and the
  clear-defer status flag; mismatch-exit mapping for the verified write
  floor; bd 1.1.0's unknown-guard exit 1 maps to an unsupported write
  contract; non-zero-exit and timeout failure paths preserve last-good;
  write deadline stays distinct from the read deadline; generation fence
  discards a stale refresh; detail caps, hidden counts, and derived-queue
  field; detail parsing across bd's three envelope shapes; write gating
  rejects remote/viewer writes.
- Client unit: panel build from a detail fixture (empty-field row omission,
  closed-issue state row with no write verbs, blocked upstream nodes,
  comment clamp with expand-in-place, hidden-count line, viewer read-only
  presentation); palette contrast floors on the panel's grounds; editor
  commit matrix (Enter, blur, Esc, click-elsewhere, modifier-Enter); ID
  click-copy and dependent-navigation intents; drag state machine (arm
  threshold, source restrictions — Done/Blocked cards not liftable,
  optimistic overlay, revert, rejected targets, hover-board hold-open
  during drag); write-timeout converge and post-reconnect reconciliation of
  optimistic state; anchor/clamp at named sample points (min/max board
  height, 0.8×/1.6× text scale, 400px floor, 70% max height with internal
  scroll); panel re-anchor on lane change; per-workspace keying.
  The completed editor spike separately proves focus acquisition,
  paint-time `ElementInputHandler` registration, committed text delivery, and
  full PTY-encoder exclusion while editor focus is armed.
- Protocol: round-trip tests for every new message.
- Functional e2e (real bd, `--network none`): bd 1.1.0 keeps write controls
  inert with the measured unsupported-contract result. After the image moves
  to a verified guard-capable write floor: open panel from a click; one write
  per verb family proven by re-running `bd show` (mandatory, not optional);
  claim/close/undo (undo restores open within the 5s window); comment; a
  seeded guard race surfacing precondition-failed; drag
  Ready→In progress recording the claim in bd, drag→Done recording close
  plus the board-side undo, drag Backlog→Ready clearing a seeded defer; a
  seeded blocked issue set to open landing in Blocked with the
  classifier-won notice; rejected drops writing nothing; PTY isolation — a
  mouse-reporting app below receives none of a drag's press/move/release
  and an armed editor's keystrokes never reach the PTY; issue-vanished
  panel closure.
- Visual e2e (injected snapshots and detail fixtures, no bd): the panel
  judged against `.impeccable/mocks/beads-card-detail.html` via the
  enumerated assertion inventory at text scale 1.0 (including the comment
  fold and hover copy glyph); a fixture set covering loading, closed,
  blocked, comment-clamped, and hidden-count variants; drag ghost within
  3px of the pointer at each `xdotool --sync` waypoint with terminal-row
  stability; no-drop lane dim; the 120ms open animation's end state equals
  the asserted panel.
- Constitution 3: every story's verification path is harness-reachable and
  named above; constitution 4's budget is verified by the waypoint test plus
  the reviewed O(1) drag update. `lat check` gates the docs.

## Risks

- The inline editor is the largest work item and easy to underestimate (the
  settings editor is entity-woven, not reusable). Mitigation: slice (a)
  ships value first; the editor spike runs in parallel with slice (a),
  registering a minimal `ElementInputHandler` for the panel in the main
  window before slice (b) commits to layout.
- bd write latency (Dolt commit + export) vs the deadline. Mitigation: 15s
  write deadline and converge-on-timeout (forced refresh + detail re-read).
  The bd 1.1.0 contract probe measured 20 attempts below the deadline, with
  an observed maximum of 972ms.
- bd 1.1.0 lacks both conditional update flags, so it cannot safely serve
  concurrency-sensitive writes. Mitigation: keep `beads_write` unavailable
  until the functional image pins and verifies a guard-capable release;
  detail reads and the read-only panel remain independently shippable.
- Ghost/clipping behavior of GPUI DnD inside a region-clipped strip.
  Mitigation: titlebar precedent plus a slice-(c) spike before polishing.
- Optimistic-revert flicker from stale refreshes. Mitigation: the
  generation fence, unit-tested.
- Live host server skew (cannot restart it). Mitigation: capability bit;
  the board degrades to today's read-only behavior.
- Rollback: all changes are additive protocol plus board/panel-local
  modules; reverting removes the UI without data loss.

## Sequencing

Order is expressed as dependency edges; no step codes. Client items depend
on protocol types and fixtures, not on server internals, so they
parallelize with server work inside each slice.

- Completed first node: the bd 1.1.0 contract spike proved unknown guards
  reject with exit 1 and preserve state, dependent ids need
  `--include-dependents`, comment shapes match the probe record, and all 20
  timed attempts stayed below 15s. This unblocks slice a but leaves slice b
  dependent on selecting and probing a guard-capable bd write floor.
- Slice a (read): detail protocol messages + `beads_detail` capability →
  { server detail fetch/caps/derived-queue/gating ∥ read-only panel entity
  split into: panel layout + palette; anchor/clamp/max-height/re-anchor;
  open animation; loading/vanish states; copy + dependent navigation } →
  seeded writable bd fixture + visual detail fixture set → visual panel
  contract + functional detail test → slice-a lat.md update + lat check.
  In parallel: the editor input-handler spike.
- Slice b (write, after the guard-capable bd floor is pinned): write protocol
  messages + `beads_write` capability →
  server write verbs (guards, clear-defer, deadline, fence, push, logging,
  bd-too-old probe) ∥ editor entity + key routing (from the spike) →
  edit surfaces as separate items: text-field editors; priority + type pick
  rows; status rail writes; claim/close/undo; labels; comment composer;
  failure notices + timeout converge + reconnect reconciliation →
  functional persistence suite → slice-b lat.md update + lat check.
- Slice c (drag, drop-commit needs slice b's server verbs; the state
  machine and ghost need only slice a): drag state machine + source
  restrictions ∥ ghost + no-drop affordances + PTY-forwarding gate +
  hover hold-open → optimistic overlay/revert wired to write results →
  functional drag verbs (all drop cases) + visual ghost tracking →
  slice-c lat.md update + lat check.
- Final (after all): cross-cutting lat.md consolidation, spec sync to
  as-built, full lat check.

## Backlog Refinement

None — the run has no backlog inputs; no P4 sources exist for this feature.

## Alignment fixes applied

- Split the capability bit into `beads_detail` / `beads_write` so a server
  never advertises writes it cannot handle (B, must).
- Defined slice a as the full mock anatomy with inert write affordances —
  the same presentation viewers and detail-only servers get — so the visual
  contract is complete from slice a (B, must).
- Un-serialized client work from server work and split the three oversized
  sequencing edges into bead-sized items (B, must/should).
- Added the bd 1.1.0 contract spike as the first node, folding in the
  write-latency measurement with a concrete method (B, must; B12).
- Added SetStatus clear-defer so the Ready drop is expressible; verb→argv
  test named (A, must).
- Added the bd version probe / typed bd-too-old error to components, tests,
  and sequencing (A+B, must).
- Added hover-board hold-open during drag to plan and client tests (A,
  must).
- Detail response now carries the server-derived queue and derivation basis
  (A, must).
- Spec-synced Story 3's visual AC (no fake-bd in the visual suite; verb
  proof lives in functional) and the Constraints verb list (no direct
  assignee verb, reasonless close, uncached detail, fake-bd scripts are
  server unit fixtures only) (A19/A20, B10).
- Named per-slice lat.md + lat check nodes; seeded writable bd fixture and
  visual detail fixture set are explicit work items (B, should).
- Pinned numbers: ghost within 3px per waypoint, 5s undo window, 70% max
  height, 120ms open animation, 400px panel floor (B, should).
- Added test coverage for: closed-issue no-verbs panel, viewer read-only
  presentation, non-zero-exit and timeout failure paths with converge,
  PTY isolation (drag and editor), drag source restrictions, drag-to-Done
  and drag-to-Ready functional cases, blocked-classifier-won case, ID
  copy + dependent navigation, expand-in-place comments + hover glyph
  visual assertions, max-height scroll clamp, reconnect reconciliation,
  re-anchor on lane change, guards-from-fresh-detail (uncached) (A5-A18).
