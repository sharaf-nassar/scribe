//! Pure A2 lane presentation and adaptive layout model.
//!
//! `specs/028-beads-board-contract.md` is the canonical prose contract;
//! `.impeccable/mocks/beads-board-directions.html`'s **A2 · Ledger + rail**
//! section (and only that section, plus A3, neither owned here) is the
//! normative mock. This module derives what a later renderer paints --
//! visible rows, queue totals, void copy, the Ready blocked-count hint, a
//! shared-epic head, per-row metadata placement, collapsed-tab descriptors,
//! pinned-lane tracks, and width allocation -- from one board snapshot plus
//! lane state, board width, height, and text scale. It contains zero GPUI
//! types or paint calls, so every geometry decision here is a plain
//! `#[test]` away from a window: painting A2 from this model is a later
//! bead's job.
//!
//! Lane state (which of Blocked/Done, if either, is pinned open) is taken as
//! input from its one owner, [`crate::beads_board::BeadsBoards`]
//! (`collapsed_lane_state`); this module never tracks or mutates it.
//!
//! Geometry constants below are mirrored from
//! `.impeccable/mocks/a2a3-contract.json`'s `geometry.a2` block (the
//! generated machine contract `scribe-zwtv.2` landed) rather than
//! re-transcribed from the mock by hand; `tests::manifest` deserializes that
//! committed JSON with `include_str!` and asserts every constant still
//! matches it, so the two cannot silently drift apart.

use scribe_common::protocol::{BeadsBoardItem, BeadsBoardSnapshot, BeadsIssueQueue};

use crate::beads_board::{CollapsedLaneState, short_epic};

// ---- Geometry, mirrored from `a2a3-contract.json`'s `geometry.a2` block ----

/// A row's total height: 19px title line + 15px subline + 4px interline gap,
/// plus centring slack the row's own `align-content` absorbs (A2-G4).
pub const ROW_H: f32 = 51.0;
pub const ROW_TITLE_H: f32 = 19.0;
pub const ROW_SUB_H: f32 = 15.0;
pub const ROW_INTERLINE_GAP: f32 = 4.0;
/// Row grid: 20px priority column + 6px gap before the title (A2-G5).
pub const ROW_PRIORITY_W: f32 = 20.0;
pub const ROW_PRIORITY_GAP: f32 = 6.0;
/// Minimum gap the sub line's right-aligned epic keeps from the centred age
/// (A2-G5).
pub const EPIC_SEPARATION_MIN: f32 = 12.0;
/// A collapsed Blocked/Done rail tab's fixed width (A2-G7).
pub const TAB_W: f32 = 36.0;
/// The hover/focus drawer's own bounds (A2-G8): an overlay laid over the
/// lanes rather than a track that joins their flex row, so opening it never
/// reflows them (A2-I1). `top`/`bottom` leave it short of the strip's own
/// headband and floor; `right` clears the still-visible rail tabs beside it.
pub const DRAWER_TOP: f32 = 5.0;
pub const DRAWER_BOTTOM: f32 = 4.0;
pub const DRAWER_RIGHT: f32 = 96.0;
pub const DRAWER_W: f32 = 452.0;
pub const DRAWER_PAD_H: f32 = 13.0;
pub const DRAWER_BORDER_W: f32 = 1.0;
pub const DRAWER_RADIUS: f32 = 3.0;
/// The native drag ghost's own box (A2-S4): a compact 320x36 card carrying
/// the row grammar it was lifted from, not the raised card A2 replaced.
pub const GHOST_W: f32 = 320.0;
pub const GHOST_H: f32 = 36.0;
pub const GHOST_PAD_LEFT: f32 = 8.0;
pub const GHOST_PAD_RIGHT: f32 = 10.0;
pub const GHOST_RADIUS: f32 = 3.0;
/// What the lifted row itself fades to while its ghost is in flight
/// (A2-S4's "source dims").
pub const DRAG_SOURCE_OPACITY: f32 = 0.38;
/// One lane head's own height (A2-G3).
pub const HEAD_H: f32 = 17.0;
/// A lane head's state seam (A2-G3).
pub const SEAM_H: f32 = 2.0;
/// The header hairline band across the whole strip: `LANES_PADDING_TOP +
/// HEAD_H + SEAM_H` (A2-G1, A2-G3).
pub const HEADBAND_H: f32 = 24.0;
/// The strip's own bottom resize grip (A2-G1, A2-G9).
pub const FLOOR_H: f32 = 3.0;
pub const LANES_PADDING_TOP: f32 = 5.0;
pub const LANES_PADDING_RIGHT: f32 = 10.0;
pub const LANES_PADDING_BOTTOM: f32 = 7.0;
/// The left control gutter the text-size steppers sit in (A2-G1, A2-R1).
pub const LANES_PADDING_LEFT: f32 = 44.0;
/// Gap between adjacent lane/tab tracks (A2-G1, A2-R1).
pub const TRACK_GAP: f32 = 16.0;
/// Borderless text-size stepper geometry (A2-G2).
pub const ZOOM_LEFT: f32 = 8.0;
pub const ZOOM_TOP: f32 = 5.0;
pub const ZOOM_GLYPH_W: f32 = 12.0;
pub const ZOOM_GLYPH_H: f32 = 17.0;
pub const ZOOM_GAP: f32 = 1.0;
/// Overflow-chevron geometry (A2-G9).
pub const CHEV_SIZE: f32 = 10.0;
pub const CHEV_RIGHT: f32 = 1.0;
pub const CHEV_BOTTOM: f32 = 0.0;
/// Centered floor-grip geometry (A2-G9).
pub const FLOOR_GRIP_W: f32 = 34.0;
pub const FLOOR_GRIP_H: f32 = 1.0;
pub const FLOOR_GRIP_TOP: f32 = 1.0;
/// Flow paint geometry (A3-G1, A3-G8).
pub const FLOW_BAND_HEIGHT: f32 = 34.0;
pub const FLOW_RULER_HEIGHT: f32 = 15.0;
pub const FLOW_GRAPH_HEIGHT: f32 = 139.0;
pub const FLOW_GRAPH_TOP: f32 = 49.0;
pub const FLOW_HBAR_TOP: f32 = 188.0;
pub const FLOW_HBAR_HEIGHT: f32 = 2.0;
pub const FLOW_PROGRESS_WIDTH: f32 = 150.0;
pub const FLOW_FADE_WIDTH: f32 = 48.0;
pub const FLOW_CHIP_OFFSET_X: f32 = 14.0;
pub const FLOW_CHIP_GAP_Y: f32 = 6.0;
/// Narrowest region that keeps the fixed drawer to the right of the fixed
/// text-control gutter: `44 + 452 + 96` (A2-G2, A2-G8). Tests use this
/// derived floor for the all-state narrow matrix rather than inventing a
/// breakpoint-specific renderer.
pub const MIN_BOARD_W: f32 = LANES_PADDING_LEFT + DRAWER_W + DRAWER_RIGHT;
/// A pinned Blocked/Done lane's share of an active lane's width, from
/// `specs/028-beads-board-contract.md`'s "Narrow-region allocation" closed
/// decision (corroborated by the mock's own `.85fr` pinned-state track). Not
/// part of the generated machine contract: `gen-contract.py` extracts no
/// per-state `grid-template-columns` values, only structural CSS.
const PINNED_LANE_SHARE: f32 = 0.85;

/// The rail always has exactly 5 tracks -- Backlog, Ready, In progress, and
/// one cell each for Blocked/Done, whichever of tab or pinned-lane width they
/// currently carry -- so there are always 4 gaps between them regardless of
/// pin state: pinning replaces a tab's cell with a wider one, it never adds a
/// cell (the mock's `grid-template-columns` always lists exactly 5 track
/// sizes, pinned or not).
const RAIL_GAPS: f32 = TRACK_GAP * 4.0;

/// Vertical chrome reserved outside the row list -- the headband, the lane's
/// own bottom padding, and the floor -- held fixed regardless of text scale
/// while only the repeating row unit scales. This mirrors how
/// `check-contract.py`'s A3 `row_capacity` formula holds `graph_h` fixed and
/// scales only the node/row-gap unit; A2 has no per-scale row-count table of
/// its own to check against, so this split is calibrated against the one
/// number the manifest does give: `visible_row_count` must reproduce
/// `body_rows: 3` at the default 197px strip and 1.0 scale (see
/// `tests::manifest`).
const VERTICAL_CHROME: f32 = HEADBAND_H + LANES_PADDING_BOTTOM + FLOOR_H;

/// Upper bound on rows a `visible_row_count` search considers. Generous: even
/// a very tall, very small-scale board falls far short of it, and the search
/// stops as soon as a row no longer fits.
const MAX_ROWS: usize = 256;

/// Which of the two collapsible queues, if either, is pinned open as a full
/// lane right now. Sourced from
/// [`crate::beads_board::BeadsBoards::collapsed_lane_state`] for Blocked and
/// Done; this module takes lane state as input and never owns or mutates it.
/// `Tab` and `Open` (a transient hover/focus drawer, which overlays the lanes
/// without reflowing them per A2-I1) allocate identical track width; only
/// `Pinned` changes the rail's width split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailState {
    pub blocked: CollapsedLaneState,
    pub done: CollapsedLaneState,
}

/// Everything [`layout`] needs: one board snapshot, the rail's collapsed-lane
/// state, and the viewport it is laid out into.
#[derive(Clone, Copy)]
pub struct A2Input<'a> {
    pub snapshot: &'a BeadsBoardSnapshot,
    pub rail: RailState,
    pub board_width: f32,
    pub board_height: f32,
    pub text_scale: f32,
}

/// A lane's copy for holding nothing, queue-specific per A2-S5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoidCopy {
    pub headline: &'static str,
    /// Ready's subordinate blocked-count hint (A2-S5); every other queue's
    /// void copy is a headline alone.
    pub subordinate: Option<String>,
}

/// One visible row: the source item plus this row's own resolved epic text
/// (already short-formed via [`crate::beads_board::short_epic`]), `None` when
/// the lane hoisted a shared epic to its head (A2-S8) or the item carries
/// none. `id` and age (from `item.updated_at`, via [`compact_relative_age`])
/// are always shown regardless of `epic`: the sub line's three columns are
/// independent slots, not a run-on string, so the age lands on the row's true
/// centre whether or not a row carries an epic (A2-G5).
#[derive(Debug)]
pub struct RowView<'a> {
    pub item: &'a BeadsBoardItem,
    pub epic: Option<String>,
}

/// One queue's presentation, uniform across all five queues and across
/// collapsed/open/pinned rendering: a Blocked/Done tab reads `total`/`void`
/// (and `width`, always [`TAB_W`] when not pinned) and ignores `rows`; an
/// open drawer or a pinned lane paints `rows` too. One shape serves both, so
/// a collapsed-tab descriptor and a pinned-lane track are the same struct in
/// two different rendering modes rather than two parallel types.
#[derive(Debug)]
pub struct QueueLane<'a> {
    pub queue: BeadsIssueQueue,
    /// The queue's true total (A2-BD1's authoritative count), not merely
    /// `rows.len()` or however many items this snapshot carries -- a queue
    /// can hold more than the server's per-queue item cap.
    pub total: u32,
    /// This lane's shared epic, hoisted to the head and dropped from every
    /// row, when every item in `items` (not only the visible slice) names the
    /// same one (A2-S8). `None` for a mixed lane, which keeps per-row epic
    /// text instead.
    pub epic: Option<String>,
    /// Whole rows only, floor-clipped to [`layout`]'s shared
    /// `visible_rows` -- never a partial row (A2-S6, A2-G10).
    pub rows: Vec<RowView<'a>>,
    /// Whether this queue holds more items than `rows` shows, the board's
    /// `⌄` overflow cue (A2-S6, A2-G9).
    pub overflow: bool,
    /// Queue-specific empty-state copy, present only when `total == 0`
    /// (A2-S5).
    pub void: Option<VoidCopy>,
    /// This lane's or tab's allocated track width (A2-R1).
    pub width: f32,
}

/// The full A2 presentation for one board: shared row geometry plus every
/// queue's lane, in `BeadsBoardSnapshot`'s own field order.
#[derive(Debug)]
pub struct A2Layout<'a> {
    /// Effective rail state at this width. A persisted pin may temporarily be
    /// a `Tab` here when its 0.85 share would starve an active lane (A2-R2);
    /// the caller's stored [`RailState`] remains untouched, so widening the
    /// same region restores it automatically.
    pub rail: RailState,
    /// A row's height at the input's text scale.
    pub row_height: f32,
    /// Whole rows every lane's `rows` is clipped to (A2-G10).
    pub visible_rows: usize,
    pub backlog: QueueLane<'a>,
    pub ready: QueueLane<'a>,
    pub in_progress: QueueLane<'a>,
    pub blocked: QueueLane<'a>,
    pub done: QueueLane<'a>,
}

/// Derive the full A2 presentation from one snapshot, lane state, and
/// viewport. Infallible: every input, however narrow, short, or oddly
/// scaled, clamps to a safe (non-negative, non-overflowing, whole-row)
/// layout rather than refusing.
pub fn layout(input: A2Input<'_>) -> A2Layout<'_> {
    let A2Input { snapshot, rail, board_width, board_height, text_scale } = input;
    let visible_rows = visible_row_count(board_height, text_scale);
    let allocation = rail_allocation(snapshot, rail, board_width, text_scale);
    let widths = allocation.widths;
    let blocked_total = snapshot.blocked_total;

    A2Layout {
        rail: allocation.rail,
        row_height: ROW_H * valid_text_scale(text_scale),
        visible_rows,
        backlog: queue_lane(QueueLaneArgs {
            queue: BeadsIssueQueue::Backlog,
            total: snapshot.backlog_total,
            items: &snapshot.backlog,
            visible_rows,
            blocked_total,
            width: widths.backlog,
        }),
        ready: queue_lane(QueueLaneArgs {
            queue: BeadsIssueQueue::Ready,
            total: snapshot.ready_total,
            items: &snapshot.ready,
            visible_rows,
            blocked_total,
            width: widths.ready,
        }),
        in_progress: queue_lane(QueueLaneArgs {
            queue: BeadsIssueQueue::InProgress,
            total: snapshot.in_progress_total,
            items: &snapshot.in_progress,
            visible_rows,
            blocked_total,
            width: widths.in_progress,
        }),
        blocked: queue_lane(QueueLaneArgs {
            queue: BeadsIssueQueue::Blocked,
            total: snapshot.blocked_total,
            items: &snapshot.blocked,
            visible_rows,
            blocked_total,
            width: widths.blocked,
        }),
        done: queue_lane(QueueLaneArgs {
            queue: BeadsIssueQueue::Done,
            total: snapshot.done_total,
            items: &snapshot.done,
            visible_rows,
            blocked_total,
            width: widths.done,
        }),
    }
}

#[derive(Clone, Copy)]
struct QueueLaneArgs<'a> {
    queue: BeadsIssueQueue,
    total: u32,
    items: &'a [BeadsBoardItem],
    visible_rows: usize,
    blocked_total: u32,
    width: f32,
}

fn queue_lane(args: QueueLaneArgs<'_>) -> QueueLane<'_> {
    let QueueLaneArgs { queue, total, items, visible_rows, blocked_total, width } = args;
    let hoisted = common_epic(items);
    let visible = items.get(..items.len().min(visible_rows)).unwrap_or(items);
    let rows = visible
        .iter()
        .map(|item| RowView {
            item,
            epic: if hoisted.is_some() {
                None
            } else {
                item.parent_epic_name.as_deref().map(short_epic)
            },
        })
        .collect();

    QueueLane {
        queue,
        total,
        epic: hoisted.map(short_epic),
        rows,
        overflow: items.len() > visible_rows,
        void: (total == 0).then(|| void_copy(queue, blocked_total)),
        width,
    }
}

/// A lane's shared epic (A2-S8): `Some` only when every item shares one
/// parent-epic id, keyed by id (the stable identity) with the already-shared
/// display name returned. Considers every item this snapshot carries for the
/// queue, not only the whole-row-clipped visible slice, so a hidden
/// differently-epic'd item (beyond the visible rows) still keeps a lane from
/// hoisting -- matching the mock's own drawer example, which hoists off all
/// four items it lists even though only three are ever visible at once.
fn common_epic(items: &[BeadsBoardItem]) -> Option<&str> {
    let first = items.first()?;
    let epic_id = first.parent_epic_id.as_deref()?;
    let epic_name = first.parent_epic_name.as_deref()?;
    let shared = items.iter().all(|item| item.parent_epic_id.as_deref() == Some(epic_id));
    shared.then_some(epic_name)
}

/// Queue-specific empty-lane copy (A2-S5). Only Backlog and Ready appear in
/// the mock verbatim ("nothing waiting", "none ready to start" with its
/// subordinate blocked count); In progress, Blocked, and Done are inferred in
/// the same voice, since a real board can show any of the five with zero
/// items (a fresh project, or an emptied pinned/open collapsible lane).
fn void_copy(queue: BeadsIssueQueue, blocked_total: u32) -> VoidCopy {
    match queue {
        BeadsIssueQueue::Backlog => VoidCopy { headline: "nothing waiting", subordinate: None },
        BeadsIssueQueue::Ready => VoidCopy {
            headline: "none ready to start",
            subordinate: (blocked_total > 0).then(|| format!("{blocked_total} blocked")),
        },
        BeadsIssueQueue::InProgress => {
            VoidCopy { headline: "nothing in progress", subordinate: None }
        }
        BeadsIssueQueue::Blocked => VoidCopy { headline: "nothing blocked", subordinate: None },
        BeadsIssueQueue::Done => VoidCopy { headline: "nothing done yet", subordinate: None },
    }
}

/// This queue's head label, shared by lane heads and collapsed-tab spines so
/// both read from one literal set instead of each repeating it.
pub fn queue_name(queue: BeadsIssueQueue) -> &'static str {
    match queue {
        BeadsIssueQueue::Backlog => "Backlog",
        BeadsIssueQueue::Ready => "Ready",
        BeadsIssueQueue::InProgress => "In progress",
        BeadsIssueQueue::Blocked => "Blocked",
        BeadsIssueQueue::Done => "Done",
    }
}

/// Each of the five lane/tab tracks' allocated width (A2-R1, the closed
/// "Narrow-region allocation" decision): the 44px left gutter, 10px right
/// padding, 16px inter-track gaps, and each unpinned 36px rail tab are
/// reserved first; an empty active lane (Backlog/Ready/In progress) gets only
/// its own legible header width; the rest divides equally among nonempty
/// active lanes, with a pinned Blocked/Done lane counted as
/// [`PINNED_LANE_SHARE`] of one such share. However narrow the board, no
/// track ever goes negative or pushes the total past `board_width`: A2 never
/// gains a horizontal scrollbar.
#[derive(Debug, Clone, Copy)]
pub struct RailWidths {
    pub backlog: f32,
    pub ready: f32,
    pub in_progress: f32,
    pub blocked: f32,
    pub done: f32,
}

impl RailWidths {
    /// Every track paired with its own width, in the rail's left-to-right
    /// order -- the order [`layout`] returns its lanes in and the renderer
    /// paints them in, so a caller walking the strip cannot pair a queue with
    /// a neighbour's width.
    pub fn tracks(self) -> [(BeadsIssueQueue, f32); 5] {
        [
            (BeadsIssueQueue::Backlog, self.backlog),
            (BeadsIssueQueue::Ready, self.ready),
            (BeadsIssueQueue::InProgress, self.in_progress),
            (BeadsIssueQueue::Blocked, self.blocked),
            (BeadsIssueQueue::Done, self.done),
        ]
    }
}

/// Which queue the board-relative point (`x`, `y`) lands on, or `None` for
/// the left control gutter, an inter-track gap, the right padding, or a point
/// outside the board entirely.
///
/// This is the drag hit test (A2-I5, A2-BD6), and it resolves the pointer
/// through the same [`rail_widths`] split and drawer bounds the renderer
/// paints from: an adaptive rail can move every track under the pointer
/// between two frames, and the one thing that must never happen is a drop
/// writing to a queue other than the one it visibly landed on. A track is hit
/// over the strip's whole height, headband and floor included, since a
/// pointer above or below a lane still names that lane and nothing else.
pub fn queue_at(input: A2Input<'_>, x: f32, y: f32) -> Option<BeadsIssueQueue> {
    let A2Input { snapshot, rail, board_width, board_height, text_scale } = input;
    if !(0.0..board_width).contains(&x) || !(0.0..board_height).contains(&y) {
        return None;
    }
    let allocation = rail_allocation(snapshot, rail, board_width, text_scale);
    // An open drawer lies over the lanes without reflowing them (A2-G8,
    // A2-I1), so inside its bounds it owns the pointer rather than the track
    // it happens to cover.
    if let Some(queue) = open_drawer(allocation.rail)
        && (board_width - DRAWER_RIGHT - DRAWER_W..board_width - DRAWER_RIGHT).contains(&x)
        && (DRAWER_TOP..board_height - DRAWER_BOTTOM).contains(&y)
    {
        return Some(queue);
    }
    let mut left = LANES_PADDING_LEFT;
    for (queue, width) in allocation.widths.tracks() {
        if (left..left + width).contains(&x) {
            return Some(queue);
        }
        left += width + TRACK_GAP;
    }
    None
}

/// The one queue whose transient drawer is open, if either is (A2-I1 opens at
/// most one per workspace).
fn open_drawer(rail: RailState) -> Option<BeadsIssueQueue> {
    if rail.blocked == CollapsedLaneState::Open {
        Some(BeadsIssueQueue::Blocked)
    } else if rail.done == CollapsedLaneState::Open {
        Some(BeadsIssueQueue::Done)
    } else {
        None
    }
}

pub fn rail_widths(
    snapshot: &BeadsBoardSnapshot,
    rail: RailState,
    board_width: f32,
    text_scale: f32,
) -> RailWidths {
    rail_allocation(snapshot, rail, board_width, text_scale).widths
}

#[derive(Debug, Clone, Copy)]
struct RailAllocation {
    rail: RailState,
    widths: RailWidths,
}

/// Allocate the requested rail, temporarily replacing a persisted pin with a
/// tab when its share would make any of the three always-present active lanes
/// narrower than that lane's existing measured legible-header width. The
/// threshold therefore comes from the contract's own allocation primitive,
/// not from a second breakpoint number (A2-R2).
fn rail_allocation(
    snapshot: &BeadsBoardSnapshot,
    requested: RailState,
    board_width: f32,
    text_scale: f32,
) -> RailAllocation {
    let widths = requested_rail_widths(snapshot, requested, board_width, text_scale);
    let has_pin = requested.blocked == CollapsedLaneState::Pinned
        || requested.done == CollapsedLaneState::Pinned;
    if !has_pin || !active_lanes_starved(widths, text_scale) {
        return RailAllocation { rail: requested, widths };
    }

    let rail =
        RailState { blocked: collapse_pin(requested.blocked), done: collapse_pin(requested.done) };
    RailAllocation { rail, widths: requested_rail_widths(snapshot, rail, board_width, text_scale) }
}

fn collapse_pin(state: CollapsedLaneState) -> CollapsedLaneState {
    if state == CollapsedLaneState::Pinned { CollapsedLaneState::Tab } else { state }
}

fn active_lanes_starved(widths: RailWidths, text_scale: f32) -> bool {
    [
        (widths.backlog, queue_name(BeadsIssueQueue::Backlog)),
        (widths.ready, queue_name(BeadsIssueQueue::Ready)),
        (widths.in_progress, queue_name(BeadsIssueQueue::InProgress)),
    ]
    .into_iter()
    .any(|(width, name)| width + 0.01 < legible_header_width(name, text_scale))
}

fn requested_rail_widths(
    snapshot: &BeadsBoardSnapshot,
    rail: RailState,
    board_width: f32,
    text_scale: f32,
) -> RailWidths {
    let text_scale = valid_text_scale(text_scale);
    let board_width = if board_width.is_finite() { board_width.max(0.0) } else { 0.0 };
    let blocked_pinned = rail.blocked == CollapsedLaneState::Pinned;
    let done_pinned = rail.done == CollapsedLaneState::Pinned;
    let pinned_count = usize::from(blocked_pinned) + usize::from(done_pinned);
    let tabs = 2_usize.saturating_sub(pinned_count);

    let fixed = LANES_PADDING_LEFT + LANES_PADDING_RIGHT + RAIL_GAPS + TAB_W * count_to_f32(tabs);
    let available = (board_width - fixed).max(0.0);

    let backlog_nonempty = snapshot.backlog_total > 0;
    let ready_nonempty = snapshot.ready_total > 0;
    let in_progress_nonempty = snapshot.in_progress_total > 0;
    let n_nonempty = usize::from(backlog_nonempty)
        + usize::from(ready_nonempty)
        + usize::from(in_progress_nonempty);

    let (backlog_w, ready_w, in_progress_w, unit) = if n_nonempty == 0 && pinned_count == 0 {
        // Nothing has work and nothing is pinned: split evenly rather than
        // leaving freed width as dead space beside three legible-only heads.
        let each = available / 3.0;
        (each, each, each, each)
    } else {
        active_lane_widths(ActiveLaneWants {
            backlog: ActiveLane {
                name: queue_name(BeadsIssueQueue::Backlog),
                nonempty: backlog_nonempty,
            },
            ready: ActiveLane {
                name: queue_name(BeadsIssueQueue::Ready),
                nonempty: ready_nonempty,
            },
            in_progress: ActiveLane {
                name: queue_name(BeadsIssueQueue::InProgress),
                nonempty: in_progress_nonempty,
            },
            pinned_count,
            available,
            text_scale,
        })
    };

    RailWidths {
        backlog: backlog_w,
        ready: ready_w,
        in_progress: in_progress_w,
        blocked: if blocked_pinned { PINNED_LANE_SHARE * unit } else { TAB_W },
        done: if done_pinned { PINNED_LANE_SHARE * unit } else { TAB_W },
    }
}

/// One active (never-collapsible) lane's name and whether it holds work,
/// bundled together because [`active_lane_widths`] always needs both at once.
#[derive(Debug, Clone, Copy)]
struct ActiveLane {
    name: &'static str,
    nonempty: bool,
}

#[derive(Clone, Copy)]
struct ActiveLaneWants {
    backlog: ActiveLane,
    ready: ActiveLane,
    in_progress: ActiveLane,
    pinned_count: usize,
    available: f32,
    text_scale: f32,
}

/// This lane's share of `available` if it turns out empty: nothing, if it
/// holds work (an occupied lane's width comes from `unit` instead).
fn empty_want(lane: ActiveLane, text_scale: f32) -> f32 {
    if lane.nonempty { 0.0 } else { legible_header_width(lane.name, text_scale) }
}

/// The non-degenerate half of [`rail_widths`]: at least one active lane holds
/// work, or a lane is pinned, so there is always a nonzero total share to
/// divide the pool by.
fn active_lane_widths(wants: ActiveLaneWants) -> (f32, f32, f32, f32) {
    let ActiveLaneWants { backlog, ready, in_progress, pinned_count, available, text_scale } =
        wants;
    let n_nonempty = usize::from(backlog.nonempty)
        + usize::from(ready.nonempty)
        + usize::from(in_progress.nonempty);

    let backlog_empty_want = empty_want(backlog, text_scale);
    let ready_empty_want = empty_want(ready, text_scale);
    let in_progress_empty_want = empty_want(in_progress, text_scale);
    let empty_want_total = backlog_empty_want + ready_empty_want + in_progress_empty_want;

    let (backlog_empty_width, ready_empty_width, in_progress_empty_width, pool) =
        if empty_want_total <= available {
            (
                backlog_empty_want,
                ready_empty_want,
                in_progress_empty_want,
                available - empty_want_total,
            )
        } else if empty_want_total > 0.0 {
            // Even every empty lane's own minimum does not fit: shrink them
            // proportionally rather than letting any one go negative.
            let shrink = available / empty_want_total;
            (
                backlog_empty_want * shrink,
                ready_empty_want * shrink,
                in_progress_empty_want * shrink,
                0.0,
            )
        } else {
            (backlog_empty_want, ready_empty_want, in_progress_empty_want, available)
        };

    let shares = count_to_f32(n_nonempty) + PINNED_LANE_SHARE * count_to_f32(pinned_count);
    let unit = pool / shares;
    (
        if backlog.nonempty { unit } else { backlog_empty_width },
        if ready.nonempty { unit } else { ready_empty_width },
        if in_progress.nonempty { unit } else { in_progress_empty_width },
        unit,
    )
}

/// ponytail: a character-count estimate of an empty lane's own legible head
/// width ("their measured legible header width" per specs/028's
/// narrow-region allocation decision) -- base head padding/gap/count-digit
/// allowance plus a flat width per name character, scaled with the text --
/// rather than real glyph shaping, which this renderer-independent model has
/// no font shaper to perform. Upgrade path: replace with a GPUI text-system
/// measurement in the render bead if a header ever visibly clips.
fn legible_header_width(name: &str, text_scale: f32) -> f32 {
    const CHAR_WIDTH: f32 = 7.0;
    const BASE: f32 = 24.0;
    (BASE + count_to_f32(name.chars().count()) * CHAR_WIDTH) * valid_text_scale(text_scale)
}

fn valid_text_scale(text_scale: f32) -> f32 {
    if text_scale.is_finite() && text_scale > 0.0 { text_scale } else { 0.0 }
}

/// How many whole rows fit in `board_height` at `text_scale` (A2-G10):
/// [`VERTICAL_CHROME`] is fixed, only the row unit scales, and the result is
/// floored so a part-height final row is never shown (A2-S6). Never panics or
/// returns a nonsensical count for a degenerate scale.
pub fn visible_row_count(board_height: f32, text_scale: f32) -> usize {
    if !text_scale.is_finite() || text_scale <= 0.0 {
        return 0;
    }
    let row_h = ROW_H * text_scale;
    let available = (board_height - VERTICAL_CHROME).max(0.0);
    (1..=MAX_ROWS).take_while(|&rows| count_to_f32(rows) * row_h <= available).count()
}

/// A small integer as `f32`, saturating through `u16` rather than an `as`
/// cast: every value this module counts (rows, lanes, characters) fits
/// comfortably under `u16::MAX`, and the round trip is exact.
///
/// `pub(crate)` so the renderer can size a lane's fixed-height row box from
/// the same `visible_rows` this module already computes, instead of
/// re-deriving its own usize-to-f32 conversion.
pub(crate) fn count_to_f32(value: usize) -> f32 {
    u16::try_from(value).map_or(f32::from(u16::MAX), f32::from)
}

/// A compact age like `"9d"`, `"3h"`, or `"0m"` for an item's
/// [`BeadsBoardItem::updated_at`] (A2-BD2), or `None` for an empty or
/// unparseable timestamp -- `bd`'s data crosses a trust boundary verbatim, so
/// a malformed value degrades to no age rather than a panic or a garbage
/// string. Owned here (rather than by a later rendering bead) because every
/// A2 row needs it and there is otherwise nothing else to duplicate it.
pub fn compact_relative_age(updated_at: &str, now_epoch_s: i64) -> Option<String> {
    let then = parse_iso8601_utc(updated_at)?;
    let elapsed = (now_epoch_s - then).max(0);
    Some(if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3600)
    } else {
        format!("{}d", elapsed / 86_400)
    })
}

/// Parse a `bd`-shaped UTC timestamp (`YYYY-MM-DDTHH:MM:SS(.fff)?Z`, the only
/// shape the server ever emits) into Unix epoch seconds.
///
/// ponytail: only a trailing literal `Z` is accepted; ceiling is that a
/// numeric-offset timestamp (`+02:00`) never parses. Upgrade path: parse and
/// apply a numeric offset if `bd` ever emits one.
fn parse_iso8601_utc(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.splitn(3, '-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let time = time.split('.').next()?;
    let mut time_parts = time.splitn(3, ':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    // Seconds allow 60 to tolerate a rare leap second without hard-failing.
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil (Y-M-D) date.
/// Howard Hinnant's `days_from_civil` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>): correct for any
/// year, including leap years, with no calendar table. `tests::civil_dates`
/// checks it against three independently well-known epoch values.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beads_board::BEADS_BOARD_HEIGHT;

    fn snapshot_with(
        backlog: u32,
        ready: u32,
        in_progress: u32,
        blocked: u32,
        done: u32,
    ) -> BeadsBoardSnapshot {
        BeadsBoardSnapshot {
            backlog_total: backlog,
            ready_total: ready,
            in_progress_total: in_progress,
            blocked_total: blocked,
            done_total: done,
            ..Default::default()
        }
    }

    fn all_tabs() -> RailState {
        RailState { blocked: CollapsedLaneState::Tab, done: CollapsedLaneState::Tab }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    fn item(id: &str, epic_id: Option<&str>, epic_name: Option<&str>) -> BeadsBoardItem {
        BeadsBoardItem {
            id: id.to_owned(),
            title: format!("issue {id}"),
            priority: 2,
            blocker_ids: Vec::new(),
            parent_epic_name: epic_name.map(str::to_owned),
            parent_epic_id: epic_id.map(str::to_owned),
            updated_at: String::new(),
        }
    }

    // ---- sparse real state (the mock's "Collapsed -- real state") --------

    #[test]
    fn sparse_real_state_gives_empty_lanes_their_own_legible_width() {
        let snapshot = snapshot_with(0, 0, 2, 4, 559);
        let widths = rail_widths(&snapshot, all_tabs(), 1200.0, 1.0);

        assert!(widths.backlog > 0.0 && widths.ready > 0.0);
        // "In progress" is a longer head label than "Ready": a content-aware
        // empty-lane width is not one flat constant shared by every queue.
        assert!(
            widths.ready < legible_header_width("In progress", 1.0),
            "an empty Ready lane should not need as much as In progress's own label"
        );
        assert!(close(widths.blocked, TAB_W) && close(widths.done, TAB_W));
        // The sole nonempty active lane takes the entire remaining pool.
        let fixed = LANES_PADDING_LEFT + LANES_PADDING_RIGHT + RAIL_GAPS + TAB_W * 2.0;
        let pool = 1200.0 - fixed - widths.backlog - widths.ready;
        assert!(close(widths.in_progress, pool));
    }

    #[test]
    fn sparse_real_state_layout_matches_the_mock_content() {
        let snapshot = BeadsBoardSnapshot {
            backlog_total: 0,
            ready_total: 0,
            in_progress_total: 2,
            blocked_total: 4,
            done_total: 559,
            in_progress: vec![
                item("2a8z.3", Some("2a8z"), Some("pi-ai-integration")),
                item("2a8z.2", Some("2a8z"), Some("pi-ai-integration")),
            ],
            ..Default::default()
        };
        let out = layout(A2Input {
            snapshot: &snapshot,
            rail: all_tabs(),
            board_width: 1200.0,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.0,
        });

        assert_eq!(out.backlog.void.as_ref().map(|v| v.headline), Some("nothing waiting"));
        assert_eq!(out.ready.void.as_ref().unwrap().headline, "none ready to start");
        assert_eq!(out.ready.void.as_ref().unwrap().subordinate.as_deref(), Some("4 blocked"));
        assert!(out.backlog.void.as_ref().unwrap().subordinate.is_none());
        // Shared epic hoists to the lane head and drops from both rows.
        assert_eq!(out.in_progress.epic.as_deref(), Some("pi-ai-integration"));
        assert_eq!(out.in_progress.rows.len(), 2);
        assert!(out.in_progress.rows.iter().all(|row| row.epic.is_none()));
        assert!(!out.in_progress.overflow);
        assert_eq!(out.blocked.total, 4);
        assert_eq!(out.done.total, 559);
    }

    // ---- busy state with one pinned lane (the mock's "Blocked pinned") ---

    #[test]
    fn pinned_blocked_busy_state_uses_three_equal_shares_plus_a_pinned_share() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        let rail = RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab };
        let widths = rail_widths(&snapshot, rail, 1200.0, 1.0);

        assert!(close(widths.backlog, widths.ready));
        assert!(close(widths.ready, widths.in_progress));
        assert!(close(widths.blocked, PINNED_LANE_SHARE * widths.backlog));
        assert!(close(widths.done, TAB_W));

        // `widths.done` already carries its own (unpinned) TAB_W, so the
        // fixed reservation here is only padding and gaps, not a second tab.
        let fixed = LANES_PADDING_LEFT + LANES_PADDING_RIGHT + RAIL_GAPS;
        let total = widths.backlog
            + widths.ready
            + widths.in_progress
            + widths.blocked
            + widths.done
            + fixed;
        assert!(close(total, 1200.0), "no horizontal scrollbar: tracks fill the board exactly");
    }

    #[test]
    fn mixed_epics_keep_per_row_epic_text() {
        let snapshot = BeadsBoardSnapshot {
            blocked_total: 4,
            blocked: vec![
                item("sc-91", Some("beads-integration"), Some("beads-integration")),
                item("sc-58", None, None),
                item("sc-36", None, None),
                item("sc-18", None, None),
            ],
            ..Default::default()
        };
        let rail = RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab };
        let out = layout(A2Input {
            snapshot: &snapshot,
            rail,
            board_width: 1200.0,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.0,
        });

        assert!(
            out.blocked.epic.is_none(),
            "not every row shares an epic, so the lane does not hoist one"
        );
        assert_eq!(out.blocked.rows[0].epic.as_deref(), Some("beads-integration"));
        assert!(out.blocked.rows[1..].iter().all(|row| row.epic.is_none()));
    }

    // ---- overflow cue: only whole rows show, `⌄` marks the rest ---------

    #[test]
    fn overflow_marks_a_lane_holding_more_than_the_visible_rows() {
        let items: Vec<BeadsBoardItem> =
            (0..5).map(|n| item(&format!("sc-{n}"), None, None)).collect();
        let snapshot =
            BeadsBoardSnapshot { backlog_total: 5, backlog: items, ..Default::default() };
        let out = layout(A2Input {
            snapshot: &snapshot,
            rail: all_tabs(),
            board_width: 1200.0,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.0,
        });

        assert_eq!(out.visible_rows, 3);
        assert_eq!(out.backlog.rows.len(), 3, "whole rows only, never a partial fourth row");
        assert!(out.backlog.overflow, "5 items over 3 visible rows must raise the overflow cue");
        assert!(out.backlog.void.is_none(), "a nonempty lane never carries void copy");
    }

    // ---- row count: whole rows only, never a partial final row -----------

    #[test]
    fn three_whole_rows_at_default_height_and_scale_one() {
        assert_eq!(visible_row_count(BEADS_BOARD_HEIGHT, 1.0), 3);
    }

    #[test]
    fn row_scale_0_8_1_0_1_6_recompute_row_count_without_changing_board_height() {
        let at_0_8 = visible_row_count(BEADS_BOARD_HEIGHT, 0.8);
        let at_1_0 = visible_row_count(BEADS_BOARD_HEIGHT, 1.0);
        let at_1_6 = visible_row_count(BEADS_BOARD_HEIGHT, 1.6);
        // Bigger text never buys more rows out of the same fixed strip.
        assert!(at_0_8 >= at_1_0);
        assert!(at_1_0 >= at_1_6);
        assert_eq!(at_1_0, 3);
    }

    #[test]
    fn max_scale_resize_floor_keeps_one_whole_row_at_every_supported_scale() {
        let floor = VERTICAL_CHROME + ROW_H * 1.6;
        for scale in [0.8_f32, 1.0, 1.6] {
            assert!(visible_row_count(floor, scale) >= 1, "scale {scale} lost its readable row");
            let rows = visible_row_count(floor, scale);
            assert!(
                count_to_f32(rows) * ROW_H * scale <= floor - VERTICAL_CHROME + 0.01,
                "scale {scale} exposed a partial row"
            );
        }
    }

    #[test]
    fn no_partial_final_row_across_a_height_sweep() {
        let mut height = 0.0_f32;
        while height <= 600.0 {
            for scale in [0.8_f32, 1.0, 1.3, 1.6] {
                let rows = visible_row_count(height, scale);
                let row_h = ROW_H * scale;
                let available = (height - VERTICAL_CHROME).max(0.0);
                assert!(
                    count_to_f32(rows) * row_h <= available + 0.01,
                    "rows={rows} must fit inside available={available} at height={height} scale={scale}"
                );
                assert!(
                    count_to_f32(rows + 1) * row_h > available + 0.01,
                    "one more row must not also fit, or this is not the largest whole count"
                );
            }
            height += 7.0;
        }
    }

    // ---- narrow regions: never negative, never overflowing the board -----

    #[test]
    fn rail_widths_never_go_negative_or_overflow_a_narrow_to_wide_matrix() {
        let occupancies = [
            snapshot_with(0, 0, 0, 0, 0),
            snapshot_with(0, 0, 3, 4, 1),
            snapshot_with(5, 5, 5, 0, 0),
            snapshot_with(1, 0, 1, 4, 559),
        ];
        let rails = [
            all_tabs(),
            RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab },
            RailState { blocked: CollapsedLaneState::Tab, done: CollapsedLaneState::Pinned },
            RailState { blocked: CollapsedLaneState::Open, done: CollapsedLaneState::Tab },
        ];

        let mut width = MIN_BOARD_W;
        while width <= 2000.0 {
            assert_rail_widths_fit_every_scale_and_state(&occupancies, &rails, width);
            width += 41.0;
        }
    }

    /// One board width, swept across every text scale, occupancy, and rail
    /// state combination.
    fn assert_rail_widths_fit_every_scale_and_state(
        occupancies: &[BeadsBoardSnapshot],
        rails: &[RailState],
        width: f32,
    ) {
        for scale in [0.8_f32, 1.0, 1.6] {
            for (snapshot, rail) in
                occupancies.iter().flat_map(|s| rails.iter().map(move |r| (s, *r)))
            {
                assert_rail_widths_fit(snapshot, rail, width, scale);
            }
        }
    }

    /// One matrix cell at or above the all-state narrow floor: every track is
    /// non-negative and the five tracks stay inside the board with no
    /// scrollbar.
    fn assert_rail_widths_fit(
        snapshot: &BeadsBoardSnapshot,
        rail: RailState,
        width: f32,
        scale: f32,
    ) {
        let w = rail_widths(snapshot, rail, width, scale);
        for value in [w.backlog, w.ready, w.in_progress, w.blocked, w.done] {
            assert!(value.is_finite() && value >= 0.0, "negative/NaN width at board_width={width}");
        }

        let reserved = w.backlog
            + w.ready
            + w.in_progress
            + w.blocked
            + w.done
            + RAIL_GAPS
            + LANES_PADDING_LEFT
            + LANES_PADDING_RIGHT;
        assert!(
            reserved <= width + 0.5,
            "scrollbar risk: reserved={reserved} exceeds board_width={width}"
        );
    }

    #[test]
    fn the_all_state_narrow_floor_keeps_the_drawer_clear_of_text_controls() {
        assert!(close(MIN_BOARD_W, 592.0));
        let drawer_left = MIN_BOARD_W - DRAWER_RIGHT - DRAWER_W;
        assert!(close(drawer_left, LANES_PADDING_LEFT));
    }

    #[test]
    fn a_starved_pin_auto_collapses_without_mutating_its_preference() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        let requested =
            RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab };
        let narrow = layout(A2Input {
            snapshot: &snapshot,
            rail: requested,
            board_width: MIN_BOARD_W,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.6,
        });
        assert_eq!(narrow.rail.blocked, CollapsedLaneState::Tab);
        assert_eq!(narrow.rail.done, CollapsedLaneState::Tab);
        assert_eq!(requested.blocked, CollapsedLaneState::Pinned, "the preference is input state");

        let restored = layout(A2Input {
            snapshot: &snapshot,
            rail: requested,
            board_width: 1200.0,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.6,
        });
        assert_eq!(restored.rail, requested, "widening restores the persisted pin");
    }

    #[test]
    fn every_named_a2_state_stays_inside_the_extreme_layout_matrix() {
        let empty = snapshot_with(0, 0, 0, 0, 0);
        let sparse = snapshot_with(0, 0, 2, 4, 559);
        let busy = snapshot_with(12, 4, 5, 4, 559);
        let states = [
            ("collapsed empty", &empty, all_tabs()),
            ("collapsed sparse", &sparse, all_tabs()),
            (
                "drawer",
                &sparse,
                RailState { blocked: CollapsedLaneState::Open, done: CollapsedLaneState::Tab },
            ),
            (
                "pinned busy",
                &busy,
                RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab },
            ),
            ("drag geometry", &busy, all_tabs()),
        ];
        let heights = [VERTICAL_CHROME + ROW_H * 1.6, 600.0];
        let extremes: Vec<_> = [MIN_BOARD_W, 1200.0]
            .into_iter()
            .flat_map(|width| {
                heights.into_iter().flat_map(move |height| {
                    [0.8_f32, 1.0, 1.6].into_iter().map(move |scale| (width, height, scale))
                })
            })
            .collect();

        for (name, snapshot, rail) in states {
            for &(width, height, scale) in &extremes {
                let out = layout(A2Input {
                    snapshot,
                    rail,
                    board_width: width,
                    board_height: height,
                    text_scale: scale,
                });
                assert_rail_widths_fit(snapshot, rail, width, scale);
                assert!(out.visible_rows >= 1, "{name} lost every row at {height}x{scale}");
                assert!(
                    [&out.backlog, &out.ready, &out.in_progress, &out.blocked, &out.done]
                        .into_iter()
                        .all(|lane| lane.rows.len() <= out.visible_rows),
                    "{name} exposed a partial row"
                );
            }
        }
    }

    // ---- drag hit testing: the same tracks the renderer paints -----------

    fn hit_input(snapshot: &BeadsBoardSnapshot, rail: RailState) -> A2Input<'_> {
        A2Input {
            snapshot,
            rail,
            board_width: 1200.0,
            board_height: BEADS_BOARD_HEIGHT,
            text_scale: 1.0,
        }
    }

    /// Every track's own midpoint must hit its own queue, walking the rail
    /// left to right exactly as the renderer lays it out.
    fn assert_every_track_midpoint_hits_itself(snapshot: &BeadsBoardSnapshot, rail: RailState) {
        let widths = rail_widths(snapshot, rail, 1200.0, 1.0);
        let mut left = LANES_PADDING_LEFT;
        for (queue, width) in widths.tracks() {
            assert_eq!(
                queue_at(hit_input(snapshot, rail), left + width / 2.0, 100.0),
                Some(queue),
                "{queue:?} track at {left}+{width} did not hit itself"
            );
            left += width + TRACK_GAP;
        }
    }

    #[test]
    fn every_track_hits_its_own_queue_in_every_rail_state() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        for rail in [
            all_tabs(),
            RailState { blocked: CollapsedLaneState::Pinned, done: CollapsedLaneState::Tab },
            RailState { blocked: CollapsedLaneState::Tab, done: CollapsedLaneState::Pinned },
        ] {
            assert_every_track_midpoint_hits_itself(&snapshot, rail);
        }
        // The sparse real state moves every boundary again: an empty lane
        // keeps only its header width, so a fixed five-equal-lanes split would
        // put In progress where Ready actually is.
        assert_every_track_midpoint_hits_itself(&snapshot_with(0, 0, 2, 4, 559), all_tabs());
    }

    #[test]
    fn the_collapsed_done_tab_is_hit_at_its_own_thirty_six_pixels() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        let right_edge = 1200.0 - LANES_PADDING_RIGHT;
        assert_eq!(
            queue_at(hit_input(&snapshot, all_tabs()), right_edge - TAB_W / 2.0, 100.0),
            Some(BeadsIssueQueue::Done)
        );
        assert_eq!(
            queue_at(hit_input(&snapshot, all_tabs()), right_edge - TAB_W - TRACK_GAP / 2.0, 100.0),
            None,
            "the gap between the two tabs belongs to neither"
        );
    }

    #[test]
    fn an_open_drawer_owns_the_lanes_it_covers() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        let rail = RailState { blocked: CollapsedLaneState::Tab, done: CollapsedLaneState::Open };
        let inside_x = 1200.0 - DRAWER_RIGHT - DRAWER_W / 2.0;

        assert_eq!(
            queue_at(hit_input(&snapshot, rail), inside_x, DRAWER_TOP + 1.0),
            Some(BeadsIssueQueue::Done),
            "a point inside the drawer targets the queue it previews"
        );
        // The drawer stops short of the strip's own floor, so the track it
        // covers gets the pointer back there.
        assert_eq!(
            queue_at(hit_input(&snapshot, rail), inside_x, BEADS_BOARD_HEIGHT - 1.0),
            Some(BeadsIssueQueue::InProgress)
        );
        assert_eq!(
            queue_at(hit_input(&snapshot, all_tabs()), inside_x, DRAWER_TOP + 1.0),
            Some(BeadsIssueQueue::InProgress),
            "a closed drawer never claims the pointer"
        );
    }

    #[test]
    fn the_gutter_padding_and_everything_outside_the_board_hit_nothing() {
        let snapshot = snapshot_with(12, 4, 5, 4, 559);
        let input = hit_input(&snapshot, all_tabs());
        for (x, y, what) in [
            (LANES_PADDING_LEFT - 1.0, 100.0, "the left control gutter"),
            (1200.0 - LANES_PADDING_RIGHT + 1.0, 100.0, "the right padding"),
            (-1.0, 100.0, "left of the board"),
            (1200.0, 100.0, "right of the board"),
            (600.0, -1.0, "above the board"),
            (600.0, BEADS_BOARD_HEIGHT, "below the board"),
        ] {
            assert_eq!(queue_at(input, x, y), None, "{what} must not name a queue");
        }
        // A track is hit over the strip's whole height, headband included.
        assert_eq!(queue_at(input, LANES_PADDING_LEFT + 1.0, 0.0), Some(BeadsIssueQueue::Backlog));
    }

    // ---- age formatting ----------------------------------------------------

    #[test]
    fn civil_dates_match_well_known_epoch_seconds() {
        assert_eq!(days_from_civil(1970, 1, 1) * 86_400, 0);
        assert_eq!(days_from_civil(2000, 1, 1) * 86_400, 946_684_800);
        assert_eq!(days_from_civil(2024, 1, 1) * 86_400, 1_704_067_200);
    }

    #[test]
    fn compact_relative_age_formats_known_offsets() {
        // now = 2027-01-15T12:00:00Z
        let now = days_from_civil(2027, 1, 15) * 86_400 + 12 * 3600;
        assert_eq!(compact_relative_age("2027-01-15T11:59:30Z", now).as_deref(), Some("0m"));
        assert_eq!(compact_relative_age("2027-01-15T10:00:00Z", now).as_deref(), Some("2h"));
        assert_eq!(compact_relative_age("2027-01-13T12:00:00Z", now).as_deref(), Some("2d"));
        assert_eq!(
            compact_relative_age("2027-01-15T12:00:01Z", now).as_deref(),
            Some("0m"),
            "clock skew clamps to zero"
        );
    }

    #[test]
    fn compact_relative_age_rejects_malformed_or_empty_timestamps() {
        assert_eq!(compact_relative_age("", 0), None);
        assert_eq!(compact_relative_age("not-an-iso-timestamp", 0), None);
        assert_eq!(compact_relative_age("2027-01-15T12:00:00+02:00", 0), None);
    }

    // ---- manifest consistency: consume the generated contract -------------

    mod manifest {
        use serde::Deserialize;

        use super::{
            BEADS_BOARD_HEIGHT, CHEV_BOTTOM, CHEV_RIGHT, CHEV_SIZE, DRAG_SOURCE_OPACITY,
            DRAWER_BORDER_W, DRAWER_BOTTOM, DRAWER_PAD_H, DRAWER_RADIUS, DRAWER_RIGHT, DRAWER_TOP,
            DRAWER_W, EPIC_SEPARATION_MIN, FLOOR_GRIP_H, FLOOR_GRIP_TOP, FLOOR_GRIP_W, FLOOR_H,
            FLOW_BAND_HEIGHT, FLOW_CHIP_GAP_Y, FLOW_CHIP_OFFSET_X, FLOW_FADE_WIDTH,
            FLOW_GRAPH_HEIGHT, FLOW_GRAPH_TOP, FLOW_HBAR_HEIGHT, FLOW_HBAR_TOP,
            FLOW_PROGRESS_WIDTH, FLOW_RULER_HEIGHT, GHOST_H, GHOST_PAD_LEFT, GHOST_PAD_RIGHT,
            GHOST_RADIUS, GHOST_W, HEAD_H, HEADBAND_H, LANES_PADDING_BOTTOM, LANES_PADDING_LEFT,
            LANES_PADDING_RIGHT, LANES_PADDING_TOP, ROW_H, ROW_INTERLINE_GAP, ROW_PRIORITY_GAP,
            ROW_PRIORITY_W, ROW_SUB_H, ROW_TITLE_H, SEAM_H, TAB_W, TRACK_GAP, ZOOM_GAP,
            ZOOM_GLYPH_H, ZOOM_GLYPH_W, ZOOM_LEFT, ZOOM_TOP, visible_row_count,
        };

        const MANIFEST_JSON: &str = include_str!("../../../.impeccable/mocks/a2a3-contract.json");

        #[derive(Deserialize)]
        struct Manifest {
            geometry: Geometry,
        }

        #[derive(Deserialize)]
        struct Geometry {
            a2: A2Geometry,
            a3: A3Geometry,
        }

        #[derive(Deserialize)]
        struct A2Geometry {
            strip_h: f32,
            lanes_padding_top: f32,
            lanes_padding_right: f32,
            lanes_padding_bottom: f32,
            lanes_padding_left: f32,
            track_gap: f32,
            headband_h: f32,
            head_h: f32,
            seam_h: f32,
            row_h: f32,
            row_title_h: f32,
            row_sub_h: f32,
            row_interline_gap: f32,
            row_priority_w: f32,
            row_priority_gap: f32,
            body_rows: usize,
            tab_w: f32,
            epic_separation_min: f32,
            floor_h: f32,
            drawer_top: f32,
            drawer_bottom: f32,
            drawer_right: f32,
            drawer_w: f32,
            drawer_pad_h: f32,
            drawer_border_w: f32,
            drawer_radius: f32,
            ghost_w: f32,
            ghost_h: f32,
            ghost_pad_left: f32,
            ghost_pad_right: f32,
            ghost_radius: f32,
            drag_source_opacity: f32,
            zoom_left: f32,
            zoom_top: f32,
            zoom_glyph_w: f32,
            zoom_glyph_h: f32,
            zoom_gap: f32,
            chev_size: f32,
            chev_right: f32,
            chev_bottom: f32,
            floor_grip_w: f32,
            floor_grip_h: f32,
            floor_grip_top: f32,
        }

        #[derive(Deserialize)]
        struct A3Geometry {
            band_h: f32,
            ruler_h: f32,
            graph_h: f32,
            graph_top: f32,
            hbar_top: f32,
            hbar_h: f32,
            progress_w: f32,
            fade_w: f32,
            chip_offset_x: f32,
            chip_gap_y: f32,
        }

        /// `assert_eq!` on two `f32`s trips `clippy::float_cmp`; every field
        /// here is a whole/half-integer pixel constant exactly representable
        /// in `f32`, so a tight epsilon is both lint-clean and as strict as
        /// exact equality in practice.
        fn assert_matches(label: &str, manifest_value: f32, constant: f32) {
            assert!(
                (manifest_value - constant).abs() < 1e-6,
                "{label}: manifest={manifest_value} constant={constant}"
            );
        }

        #[test]
        fn constants_match_the_generated_contract() {
            let manifest: Manifest =
                serde_json::from_str(MANIFEST_JSON).expect("valid a2a3-contract.json");
            let a2 = manifest.geometry.a2;
            let a3 = manifest.geometry.a3;

            assert_matches("strip_h", a2.strip_h, BEADS_BOARD_HEIGHT);
            assert_matches("lanes_padding_top", a2.lanes_padding_top, LANES_PADDING_TOP);
            assert_matches("lanes_padding_right", a2.lanes_padding_right, LANES_PADDING_RIGHT);
            assert_matches("lanes_padding_bottom", a2.lanes_padding_bottom, LANES_PADDING_BOTTOM);
            assert_matches("lanes_padding_left", a2.lanes_padding_left, LANES_PADDING_LEFT);
            assert_matches("track_gap", a2.track_gap, TRACK_GAP);
            assert_matches("headband_h", a2.headband_h, HEADBAND_H);
            assert_matches("head_h", a2.head_h, HEAD_H);
            assert_matches("seam_h", a2.seam_h, SEAM_H);
            assert_matches("row_h", a2.row_h, ROW_H);
            assert_matches("row_title_h", a2.row_title_h, ROW_TITLE_H);
            assert_matches("row_sub_h", a2.row_sub_h, ROW_SUB_H);
            assert_matches("row_interline_gap", a2.row_interline_gap, ROW_INTERLINE_GAP);
            assert_matches("row_priority_w", a2.row_priority_w, ROW_PRIORITY_W);
            assert_matches("row_priority_gap", a2.row_priority_gap, ROW_PRIORITY_GAP);
            assert_matches("tab_w", a2.tab_w, TAB_W);
            assert_matches("epic_separation_min", a2.epic_separation_min, EPIC_SEPARATION_MIN);
            assert_matches("floor_h", a2.floor_h, FLOOR_H);
            assert_matches("drawer_top", a2.drawer_top, DRAWER_TOP);
            assert_matches("drawer_bottom", a2.drawer_bottom, DRAWER_BOTTOM);
            assert_matches("drawer_right", a2.drawer_right, DRAWER_RIGHT);
            assert_matches("drawer_w", a2.drawer_w, DRAWER_W);
            assert_matches("drawer_pad_h", a2.drawer_pad_h, DRAWER_PAD_H);
            assert_matches("drawer_border_w", a2.drawer_border_w, DRAWER_BORDER_W);
            assert_matches("drawer_radius", a2.drawer_radius, DRAWER_RADIUS);
            assert_matches("ghost_w", a2.ghost_w, GHOST_W);
            assert_matches("ghost_h", a2.ghost_h, GHOST_H);
            assert_matches("ghost_pad_left", a2.ghost_pad_left, GHOST_PAD_LEFT);
            assert_matches("ghost_pad_right", a2.ghost_pad_right, GHOST_PAD_RIGHT);
            assert_matches("ghost_radius", a2.ghost_radius, GHOST_RADIUS);
            assert_matches("drag_source_opacity", a2.drag_source_opacity, DRAG_SOURCE_OPACITY);
            assert_matches("zoom_left", a2.zoom_left, ZOOM_LEFT);
            assert_matches("zoom_top", a2.zoom_top, ZOOM_TOP);
            assert_matches("zoom_glyph_w", a2.zoom_glyph_w, ZOOM_GLYPH_W);
            assert_matches("zoom_glyph_h", a2.zoom_glyph_h, ZOOM_GLYPH_H);
            assert_matches("zoom_gap", a2.zoom_gap, ZOOM_GAP);
            assert_matches("chev_size", a2.chev_size, CHEV_SIZE);
            assert_matches("chev_right", a2.chev_right, CHEV_RIGHT);
            assert_matches("chev_bottom", a2.chev_bottom, CHEV_BOTTOM);
            assert_matches("floor_grip_w", a2.floor_grip_w, FLOOR_GRIP_W);
            assert_matches("floor_grip_h", a2.floor_grip_h, FLOOR_GRIP_H);
            assert_matches("floor_grip_top", a2.floor_grip_top, FLOOR_GRIP_TOP);
            assert_matches("a3.band_h", a3.band_h, FLOW_BAND_HEIGHT);
            assert_matches("a3.ruler_h", a3.ruler_h, FLOW_RULER_HEIGHT);
            assert_matches("a3.graph_h", a3.graph_h, FLOW_GRAPH_HEIGHT);
            assert_matches("a3.graph_top", a3.graph_top, FLOW_GRAPH_TOP);
            assert_matches("a3.hbar_top", a3.hbar_top, FLOW_HBAR_TOP);
            assert_matches("a3.hbar_h", a3.hbar_h, FLOW_HBAR_HEIGHT);
            assert_matches("a3.progress_w", a3.progress_w, FLOW_PROGRESS_WIDTH);
            assert_matches("a3.fade_w", a3.fade_w, FLOW_FADE_WIDTH);
            assert_matches("a3.chip_offset_x", a3.chip_offset_x, FLOW_CHIP_OFFSET_X);
            assert_matches("a3.chip_gap_y", a3.chip_gap_y, FLOW_CHIP_GAP_Y);
            assert_eq!(a2.body_rows, 3);
            assert_eq!(
                visible_row_count(a2.strip_h, 1.0),
                a2.body_rows,
                "visible_row_count's fixed-chrome/scaled-row split must reproduce the contract's default row count"
            );
        }
    }
}
