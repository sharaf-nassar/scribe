//! Constellation workspace board: compact five-column Beads state for GPUI.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    Anchor, AnyElement, Bounds, Context, DragMoveEvent, FocusHandle, FontWeight, KeyDownEvent,
    MouseButton, Pixels, Point, Render, Rgba, Role, SharedString, Window, anchored, div,
    linear_color_stop, linear_gradient, point, prelude::*, px,
};

use crate::beads_board_a2::{
    self, A2Input, QueueLane, RailState, RowView, VoidCopy, compact_relative_age, count_to_f32,
    queue_name,
};
use crate::beads_flow::{FlowLayout, FlowNodeControl, FlowRender, FlowTrace, layout_flow};
use crate::beads_panel::BeadsPanels;
use crate::layout::Rect;
use crate::opacity::surface;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{
    BeadsBoardItem, BeadsBoardSnapshot, BeadsBoardState, BeadsEpicGraph, BeadsEpicGraphOutcome,
    BeadsIssueQueue, BeadsIssueWriteResult,
};

/// One of the things whose hover keeps a board open. They overlap, so each is
/// tracked on its own rather than as a single flag.
#[derive(Clone, Copy)]
pub enum HoverSource {
    /// The workspace's bead in the titlebar or a region bar.
    Bead = 1,
    /// The board itself.
    Board = 2,
    /// A control inside the board, which takes hover away from the board.
    Control = 4,
}

/// One of the things whose pointer hover or keyboard focus keeps a collapsed
/// lane's drawer open. Tab and drawer overlap while the pointer crosses
/// between them, so each is tracked on its own — the lane-level twin of
/// `HoverSource`.
#[derive(Clone, Copy)]
pub enum LaneHoverSource {
    /// The collapsed rail tab, by pointer hover.
    Tab = 1,
    /// The drawer the tab opened, by pointer hover.
    Drawer = 2,
    /// Keyboard focus on the tab, or inside the drawer it opened.
    Focus = 4,
}

/// What a collapsed queue's lane looks like right now, for the rail and
/// drawer a later bead paints from this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapsedLaneState {
    /// A plain 36px tab: not pinned, not hovered or focused.
    Tab,
    /// Open as a transient, non-reflowing drawer by hover or focus.
    Open,
    /// Pinned open as a full lane.
    Pinned,
}

/// Reader-owned snapshots plus GPUI-owned hover/pin intent.
///
/// Every one of these is keyed by workspace because a board is a region's own
/// furniture: two regions side by side each own a board, and pinning or
/// hovering one says nothing about the other.
#[derive(Debug, Clone, Default)]
pub struct BeadsBoards {
    states: HashMap<WorkspaceId, BeadsBoardState>,
    retry_after: HashMap<WorkspaceId, Instant>,
    /// Which of a board's hover sources the pointer is on, per workspace.
    /// An entry with no sources left is a board inside its grace period.
    hovered: HashMap<WorkspaceId, u8>,
    pinned: HashSet<WorkspaceId>,
    /// Pins read back from the window record, held until their region shows
    /// up. A restored pin names a workspace the layout has not adopted yet, and
    /// pruning against the live layout would drop it before it could apply.
    pending_pins: HashSet<WorkspaceId>,
    hover_expires: HashMap<WorkspaceId, Instant>,
    /// Which queue (Blocked or Done), if either, a workspace has pinned its
    /// collapsed lane open. At most one: pinning one queue replaces the
    /// other, the one-pinned-lane rule the mock draws.
    lane_pinned: HashMap<WorkspaceId, BeadsIssueQueue>,
    /// Lane pins read back from the window record, held until their region
    /// shows up — the lane-level twin of `pending_pins`.
    pending_lane_pins: HashMap<WorkspaceId, BeadsIssueQueue>,
    /// The one collapsed-lane drawer a workspace has open by hover or focus,
    /// and which of its sources still holds it open. Only one drawer is ever
    /// open per workspace (A2-I1), so entering a different queue replaces
    /// this outright rather than tracking Blocked and Done independently. An
    /// entry whose sources are all gone is a drawer inside its grace period,
    /// the same shape `hovered` gives the board itself.
    lane_hovered: HashMap<WorkspaceId, (BeadsIssueQueue, u8)>,
    lane_hover_expires: HashMap<WorkspaceId, Instant>,
    /// Text a card asked to put on the clipboard, drained by the view on the
    /// next frame. The board is built by a free function with no reach into
    /// the window's clipboard handle, so the request is parked here the way
    /// hover and pin intent already are.
    pending_copy: Option<String>,
    /// Steps away from the board's designed text size, one tenth each. Held as
    /// steps rather than a factor so the default stays a plain `Default`.
    text_scale_steps: i8,
    /// Boards dragged off the designed height, by workspace. Per workspace and
    /// not per window like the text size, because the strip a pinned board
    /// takes comes out of that region's terminal and no other's.
    heights: HashMap<WorkspaceId, f32>,
    /// The bottom-bar drag in flight, if any.
    resize: Option<BoardResize>,
    /// Eligible card press waiting for GPUI's native drag arm.
    card_press: Option<CardDragPress>,
    /// Card drag currently tracked by GPUI's native drag stream.
    card_drag: Option<CardDragState>,
    /// Cards painted in their requested lane until a write result and the
    /// authoritative board snapshot settle that request.
    optimistic_drops: HashMap<(WorkspaceId, String), OptimisticDrop>,
    /// Whether the server offered `beads_flow` on this connection. A board
    /// never enters Flow without it, so losing the bit on reconnect drops
    /// every open graph rather than leaving one nothing can refresh.
    flow_enabled: bool,
    /// The Flow strip each workspace is showing instead of its lanes.
    flows: HashMap<WorkspaceId, FlowView>,
    /// Workspaces in the order their Flow strips opened, oldest first.
    flow_open_order: VecDeque<WorkspaceId>,
    /// One in-flight epic-graph request per workspace, newest wins. Presence
    /// is the fence: mode exit, workspace loss and capability withdrawal all
    /// drop the entry, so a reply that outlived its request finds no match.
    pending_flows: HashMap<WorkspaceId, PendingFlow>,
    /// Epic-graph requests the view drains on its next frame, mirroring how
    /// the panel parks its detail requests.
    flow_requests: VecDeque<(WorkspaceId, String)>,
    /// Issue each live agent session in this window is working on right now.
    ///
    /// Keyed by session rather than by issue because the binding's lifetime is
    /// the session's: an ended session clears its own entry without disturbing
    /// another agent that happens to be on the same issue. The server already
    /// delivers these to the window's local owner alone, so every entry here
    /// is by construction live *here*.
    live_issues: HashMap<SessionId, String>,
}

/// One workspace's frozen Flow graph and the cursor it opened at.
///
/// The graph is captured once at open (Q5) and board polling never mutates
/// it: a strip that re-ranked itself under the pointer would move the node a
/// click was travelling towards.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowView {
    pub epic_id: String,
    pub cursor_issue_id: String,
    pub graph: BeadsEpicGraph,
    pub layout: FlowLayout,
    pub scroll_x: f32,
    /// Node the pointer is over, or `None` at rest. Hover is transient view
    /// state, so it never survives a mode exit or a re-opened graph.
    pub hovered_issue_id: Option<String>,
}

/// An epic-graph request waiting for its reply.
///
/// The cursor lives here as *latest intent* rather than being captured per
/// request. Two clicks on one epic therefore collapse to the second: the
/// reply carries graph content only, so honouring it against the newest
/// cursor is what makes an out-of-order delivery land on the card the user
/// actually asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFlow {
    epic_id: String,
    cursor_issue_id: String,
}

/// One board's bottom bar, held by the pointer.
///
/// The press position and the height it started from are both kept so the drag
/// stays a delta: a pointer that outruns a frame still resolves to the height
/// the gesture asked for rather than to wherever it was last sampled.
#[derive(Debug, Clone, Copy)]
struct BoardResize {
    workspace_id: WorkspaceId,
    press_y: f32,
    from_height: f32,
}

/// GPUI's pinned drag arm uses this exact value with a strict greater-than
/// comparison. Keeping the state boundary beside the board makes its unit
/// contract explicit while `on_drag` remains the runtime authority.
const CARD_DRAG_THRESHOLD: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardDragPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone)]
struct CardDragPress {
    workspace_id: WorkspaceId,
    source: BeadsBoardItem,
    source_lane: u8,
    origin: CardDragPoint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CardDragState {
    pub workspace_id: WorkspaceId,
    pub source: BeadsBoardItem,
    pub source_lane: u8,
    pub pointer: CardDragPoint,
    pub hovered_lane: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OptimisticDrop {
    source_lane: u8,
    target_lane: u8,
    generation: Option<u64>,
}

/// The visible card GPUI carries in its window-layer drag root.
#[derive(Debug, Clone, PartialEq)]
pub struct CardDragGhost {
    pub source: BeadsBoardItem,
    pub width: f32,
    pub height: f32,
}

impl BeadsBoards {
    /// Latch the connection's `beads_flow` capability.
    ///
    /// Losing it drops every open graph: the board keeps painting lanes from
    /// the snapshot it already polls, but nothing can refresh a Flow strip a
    /// reconnected server will not answer for.
    pub fn set_flow_enabled(&mut self, enabled: bool) {
        self.flow_enabled = enabled;
        if !enabled {
            self.flows.clear();
            self.flow_open_order.clear();
            self.pending_flows.clear();
            self.flow_requests.clear();
        }
    }

    /// Ask for `card`'s epic graph, if this board can enter Flow at all.
    ///
    /// The panel opens either way — this only decides whether the strip
    /// follows. A card with no epic stays in Lanes without a request, which
    /// is the Q3 rule the server's refusals also encode.
    pub fn request_card_flow(&mut self, workspace_id: WorkspaceId, card: &BeadsBoardItem) {
        if !self.flow_enabled {
            return;
        }
        let Some(epic_id) = card.parent_epic_id.clone() else { return };
        self.pending_flows.insert(
            workspace_id,
            PendingFlow { epic_id: epic_id.clone(), cursor_issue_id: card.id.clone() },
        );
        self.flow_requests.push_back((workspace_id, epic_id));
    }

    /// Drain one parked epic-graph request.
    pub fn take_flow_request(&mut self) -> Option<(WorkspaceId, String)> {
        self.flow_requests.pop_front()
    }

    /// Apply one epic-graph reply against the live fence.
    ///
    /// Returns whether the strip changed. A reply is honoured only while a
    /// request for the same epic is still outstanding, and it opens at the
    /// fence's *current* cursor so a superseded click cannot resurrect its
    /// own target. Every refusal leaves the board in Lanes.
    pub fn apply_epic_graph(
        &mut self,
        workspace_id: WorkspaceId,
        epic_id: &str,
        outcome: BeadsEpicGraphOutcome,
    ) -> bool {
        let Some(pending) = self.pending_flows.get(&workspace_id) else { return false };
        if pending.epic_id != epic_id {
            return false;
        }
        let BeadsEpicGraphOutcome::Graph(graph) = outcome else {
            self.pending_flows.remove(&workspace_id);
            return false;
        };
        let cursor_issue_id = pending.cursor_issue_id.clone();
        self.pending_flows.remove(&workspace_id);
        let Ok(layout) = layout_flow(&graph, self.text_scale()) else { return false };
        if !graph.nodes.iter().any(|node| node.id == cursor_issue_id) {
            return false;
        }
        self.flows.insert(
            workspace_id,
            FlowView {
                epic_id: epic_id.to_owned(),
                cursor_issue_id,
                graph: *graph,
                layout,
                scroll_x: 0.0,
                hovered_issue_id: None,
            },
        );
        self.flow_open_order.retain(|candidate| *candidate != workspace_id);
        self.flow_open_order.push_back(workspace_id);
        true
    }

    /// The Flow strip `workspace_id` is showing, if it is in Flow at all.
    pub fn flow(&self, workspace_id: WorkspaceId) -> Option<&FlowView> {
        self.flows.get(&workspace_id)
    }

    /// Copy out everything painting this workspace's Flow strip needs.
    ///
    /// Takes `&self` so the caller can read it through the guard it is already
    /// holding for the rest of the render pass. The strip cannot look this up
    /// for itself: painting happens inside that guard, and `Mutex` is not
    /// reentrant, so a second lock would deadlock every board — lanes
    /// included, since the lookup would precede the is-this-Flow test.
    pub fn flow_snapshot(&self, workspace_id: WorkspaceId) -> Option<FlowStripSnapshot> {
        let flow = self.flow(workspace_id)?;
        Some(FlowStripSnapshot {
            graph: flow.graph.clone(),
            layout: flow.layout.clone(),
            cursor_issue_id: flow.cursor_issue_id.clone(),
            scroll_x: flow.scroll_x,
            trace: flow
                .hovered_issue_id
                .as_deref()
                .and_then(|hovered| FlowTrace::from_hover(&flow.graph, hovered)),
            live_issue_ids: self.live_issue_ids(),
        })
    }

    /// Leave Flow and return to lanes, discarding any request in flight.
    pub fn exit_flow(&mut self, workspace_id: WorkspaceId) -> bool {
        let left = self.flows.remove(&workspace_id).is_some();
        self.flow_open_order.retain(|candidate| *candidate != workspace_id);
        self.pending_flows.remove(&workspace_id);
        left
    }

    /// Return the most recently opened Flow strip to lanes.
    ///
    /// Escape reaches this only after the detail panel has declined the key,
    /// so a focused panel always dismisses before the strip changes mode.
    pub fn exit_latest_flow(&mut self) -> bool {
        self.flow_open_order
            .back()
            .copied()
            .is_some_and(|workspace_id| self.exit_flow(workspace_id))
    }

    /// Move the Flow cursor to another node in the frozen graph.
    ///
    /// The graph and the epic are untouched: this is the board-state half of
    /// a node activation. Retargeting the detail panel from the same seam is
    /// a separate slice.
    pub fn move_flow_cursor(&mut self, workspace_id: WorkspaceId, issue_id: &str) -> bool {
        let Some(flow) = self.flows.get_mut(&workspace_id) else { return false };
        if flow.cursor_issue_id == issue_id {
            return false;
        }
        if !flow.graph.nodes.iter().any(|node| node.id == issue_id) {
            return false;
        }
        issue_id.clone_into(&mut flow.cursor_issue_id);
        true
    }

    /// Bind or clear the issue a live agent session is working on.
    ///
    /// `None` clears, which is the same frame the server sends when the agent
    /// moves on, its session ends, or its client disconnects — so a halo can
    /// never outlive the work it reports. Returns whether anything changed,
    /// so a repeated binding schedules no repaint.
    ///
    /// This is the whole liveness rule: the halo follows an exact
    /// issue-to-session join, never an assignee string. An agent Scribe
    /// cannot see leaves no entry here and therefore paints no halo.
    pub fn set_focused_issue(&mut self, session_id: SessionId, issue_id: Option<String>) -> bool {
        match issue_id {
            Some(issue_id) => {
                if self.live_issues.get(&session_id) == Some(&issue_id) {
                    return false;
                }
                self.live_issues.insert(session_id, issue_id);
                true
            }
            None => self.live_issues.remove(&session_id).is_some(),
        }
    }

    /// Issues a live session in this window is on, for the Flow renderer.
    #[must_use]
    pub fn live_issue_ids(&self) -> HashSet<String> {
        self.live_issues.values().cloned().collect()
    }

    /// Record which node the pointer is over, or clear it on leave.
    ///
    /// Returns whether anything changed, so a pointer crossing a node it is
    /// already tracing does not schedule a repaint. A leave is honoured only
    /// for the node that owns the current trace: pointers cross node borders
    /// in an arbitrary order, so an unfiltered leave from the node just
    /// departed would erase the trace the newly entered node had set.
    pub fn set_flow_hover(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        entered: bool,
    ) -> bool {
        let Some(flow) = self.flows.get_mut(&workspace_id) else { return false };
        let next = if entered {
            if !flow.graph.nodes.iter().any(|node| node.id == issue_id) {
                return false;
            }
            Some(issue_id.to_owned())
        } else if flow.hovered_issue_id.as_deref() == Some(issue_id) {
            None
        } else {
            return false;
        };
        if flow.hovered_issue_id == next {
            return false;
        }
        flow.hovered_issue_id = next;
        true
    }

    /// Scroll a Flow strip along its one axis, clamped to the graph.
    ///
    /// Flow never grows a vertical axis — a rank that will not fit the row
    /// budget fails layout instead — so a wheel anywhere over the strip moves
    /// it horizontally regardless of which way the pointer scrolled.
    pub fn scroll_flow(&mut self, workspace_id: WorkspaceId, delta_x: f32, board: Rect) -> bool {
        let Some(flow) = self.flows.get_mut(&workspace_id) else { return false };
        let span = (flow.layout.width - board.width).max(0.0);
        let next = (flow.scroll_x + delta_x).clamp(0.0, span);
        if (next - flow.scroll_x).abs() < f32::EPSILON {
            return false;
        }
        flow.scroll_x = next;
        true
    }

    /// Re-run layout for every open strip after a text-scale change.
    ///
    /// A strip whose graph no longer fits the row budget at the new scale
    /// returns to lanes rather than clipping nodes out of sight.
    fn relayout_flows(&mut self) {
        let scale = self.text_scale();
        self.flows.retain(|_, flow| {
            let Ok(layout) = layout_flow(&flow.graph, scale) else { return false };
            let span = (layout.width - flow.layout.width).min(0.0);
            flow.scroll_x = (flow.scroll_x + span).max(0.0);
            flow.layout = layout;
            true
        });
        self.flow_open_order.retain(|workspace_id| self.flows.contains_key(workspace_id));
    }

    /// Replace one server snapshot and report cards whose authoritative lane
    /// differed from both ends of an applied drop.
    pub fn update(
        &mut self,
        workspace_id: WorkspaceId,
        state: BeadsBoardState,
    ) -> Vec<(String, u8)> {
        if !matches!(state, BeadsBoardState::Unavailable { .. }) {
            self.retry_after.remove(&workspace_id);
        }
        if matches!(state, BeadsBoardState::NotDetected) {
            self.hovered.remove(&workspace_id);
            self.pinned.remove(&workspace_id);
            self.hover_expires.remove(&workspace_id);
            self.lane_pinned.remove(&workspace_id);
            self.lane_hovered.remove(&workspace_id);
            self.lane_hover_expires.remove(&workspace_id);
            self.exit_flow(workspace_id);
            if self.resize.is_some_and(|drag| drag.workspace_id == workspace_id) {
                self.resize = None;
            }
            if self.card_press.as_ref().is_some_and(|press| press.workspace_id == workspace_id)
                || self.card_drag.as_ref().is_some_and(|drag| drag.workspace_id == workspace_id)
            {
                self.end_card_drag();
            }
        }
        let snapshot = match &state {
            BeadsBoardState::Loading { cached } => cached.as_ref(),
            BeadsBoardState::Ready { snapshot, .. } => Some(snapshot),
            BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => None,
        };
        let settled: Vec<_> = self
            .optimistic_drops
            .keys()
            .filter(|(candidate, _)| *candidate == workspace_id)
            .cloned()
            .collect();
        let mut classifier_won = Vec::new();
        for key in settled {
            let Some(drop) = self.optimistic_drops.remove(&key) else { continue };
            let actual_lane = snapshot.and_then(|snapshot| snapshot_card_lane(snapshot, &key.1));
            if let Some(actual_lane) = actual_lane
                && drop.generation.is_some()
                && actual_lane != drop.source_lane
                && actual_lane != drop.target_lane
            {
                classifier_won.push((key.1, actual_lane));
            }
        }
        self.states.insert(workspace_id, state);
        classifier_won
    }

    pub fn detected(&self, workspace_id: WorkspaceId) -> bool {
        matches!(
            self.states.get(&workspace_id),
            Some(BeadsBoardState::Loading { cached: Some(_) } | BeadsBoardState::Ready { .. })
        )
    }

    pub fn state(&self, workspace_id: WorkspaceId) -> Option<&BeadsBoardState> {
        self.states.get(&workspace_id)
    }

    pub fn needs_refresh(&self, workspace_id: WorkspaceId, max_age: Duration) -> bool {
        let Some(state) = self.states.get(&workspace_id) else { return true };
        match state {
            BeadsBoardState::Ready { snapshot, stale, .. } => {
                let now: u64 = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                *stale
                    || Duration::from_millis(now.saturating_sub(snapshot.refreshed_at_epoch_ms))
                        >= max_age
            }
            BeadsBoardState::Loading { .. } => false,
            BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => true,
        }
    }

    pub fn due_retry(&mut self, after: Duration) -> Option<WorkspaceId> {
        let now = Instant::now();
        let workspace_id = self.states.iter().find_map(|(workspace_id, state)| {
            matches!(state, BeadsBoardState::Unavailable { .. })
                .then_some(*workspace_id)
                .filter(|id| self.retry_after.get(id).is_none_or(|deadline| *deadline <= now))
        })?;
        self.retry_after.insert(workspace_id, now + after);
        Some(workspace_id)
    }

    /// Every board painting this frame, each with whether it is pinned.
    ///
    /// One entry per workspace, never one per window: each region paints its
    /// own board, so hovering one region's bead while another region's board
    /// is pinned shows both, and focus enters into none of it.
    pub fn visible(&self) -> Vec<(WorkspaceId, bool)> {
        self.pinned
            .iter()
            .map(|workspace_id| (*workspace_id, true))
            .chain(
                self.hovered
                    .keys()
                    .filter(|workspace_id| !self.pinned.contains(workspace_id))
                    .map(|workspace_id| (*workspace_id, false)),
            )
            .collect()
    }

    /// Ask for `text` to be put on the clipboard.
    pub fn copy(&mut self, text: String) {
        self.pending_copy = Some(text);
    }

    /// Take the copy a card asked for, if any.
    pub fn take_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// How much bigger or smaller than designed the board's text is.
    pub fn text_scale(&self) -> f32 {
        1.0 + f32::from(self.text_scale_steps) * TEXT_SCALE_STEP
    }

    /// Nudge every board's text size, clamped to what the fixed-height strip
    /// can still show a readable row in.
    pub fn adjust_text_scale(&mut self, steps: i8) {
        self.text_scale_steps =
            (self.text_scale_steps + steps).clamp(MIN_TEXT_SCALE_STEPS, MAX_TEXT_SCALE_STEPS);
        self.relayout_flows();
    }

    /// How tall `workspace_id`'s board paints — and, while it is pinned, how
    /// much of its region it reserves.
    pub fn height(&self, workspace_id: WorkspaceId) -> f32 {
        self.heights.get(&workspace_id).copied().unwrap_or(BEADS_BOARD_HEIGHT)
    }

    /// Grab `workspace_id`'s bottom bar at `y`, in the coordinates the drag
    /// will be reported in.
    pub fn start_resize(&mut self, workspace_id: WorkspaceId, y: f32) {
        self.resize =
            Some(BoardResize { workspace_id, press_y: y, from_height: self.height(workspace_id) });
    }

    /// The board whose bar the pointer is holding, if any.
    pub fn resizing(&self) -> Option<WorkspaceId> {
        self.resize.map(|drag| drag.workspace_id)
    }

    /// Take the drag to `y`, keeping the board between one readable issue row
    /// and `max`. Reports whether the height actually moved.
    pub fn resize_to(&mut self, y: f32, max: f32) -> bool {
        let Some(drag) = self.resize else { return false };
        let floor = self.min_height();
        let height = (drag.from_height + y - drag.press_y).clamp(floor, max.max(floor));
        let moved = (height - self.height(drag.workspace_id)).abs() > f32::EPSILON;
        self.heights.insert(drag.workspace_id, height);
        moved
    }

    /// Let go of the bar, reporting whether a drag was in flight.
    pub fn end_resize(&mut self) -> bool {
        self.resize.take().is_some()
    }

    /// Arm one eligible card press. GPUI decides whether it becomes a drag;
    /// Done and Blocked never register that native drag in the first place.
    pub fn arm_card_drag(
        &mut self,
        workspace_id: WorkspaceId,
        source: BeadsBoardItem,
        source_lane: u8,
        origin: CardDragPoint,
    ) -> bool {
        self.card_press = None;
        self.card_drag = None;
        if !card_drag_source(source_lane) {
            return false;
        }
        self.card_press = Some(CardDragPress { workspace_id, source, source_lane, origin });
        true
    }

    /// Promote the armed press once GPUI's native `on_drag` fires.
    pub fn start_card_drag(&mut self, pointer: CardDragPoint, board: Rect) -> bool {
        let Some(press) = self.card_press.as_ref() else { return false };
        let travel = (pointer.x - press.origin.x).hypot(pointer.y - press.origin.y);
        if travel <= CARD_DRAG_THRESHOLD {
            return false;
        }
        self.card_drag = Some(CardDragState {
            workspace_id: press.workspace_id,
            source: press.source.clone(),
            source_lane: press.source_lane,
            pointer,
            hovered_lane: card_drag_lane(board, pointer),
        });
        self.card_press = None;
        true
    }

    /// Store one native drag move in constant work; no request or subprocess
    /// belongs on this path.
    pub fn update_card_drag(&mut self, pointer: CardDragPoint, board: Rect) -> bool {
        let Some(drag) = self.card_drag.as_mut() else { return false };
        drag.pointer = pointer;
        drag.hovered_lane = card_drag_lane(board, pointer);
        true
    }

    pub fn card_drag(&self) -> Option<&CardDragState> {
        self.card_drag.as_ref()
    }

    /// Rejected-lane presentation for one board, read while its caller already
    /// holds the shared store guard.
    pub fn drag_target(&self, workspace_id: WorkspaceId) -> Option<u8> {
        self.card_drag.as_ref().and_then(|drag| {
            (drag.workspace_id == workspace_id).then_some(drag.hovered_lane).flatten()
        })
    }

    /// Describe the active drag's native ghost at the source card's size.
    pub fn card_drag_ghost(&self, board: Rect, scale: f32) -> Option<CardDragGhost> {
        self.card_drag.as_ref().map(|drag| CardDragGhost::new(drag.source.clone(), board, scale))
    }

    /// Whether the board owns pointer routing before it reaches the PTY.
    pub fn blocks_pty_mouse(&self) -> bool {
        self.card_press.is_some() || self.card_drag.is_some()
    }

    /// Clear an armed press or active drag. Only an active drag reports true;
    /// a false result leaves GPUI's ordinary click path live.
    pub fn end_card_drag(&mut self) -> bool {
        self.take_card_drag().is_some()
    }

    /// Clear the press and return the completed native drag, if one lifted.
    pub fn take_card_drag(&mut self) -> Option<CardDragState> {
        self.card_press = None;
        self.card_drag.take()
    }

    /// Paint an accepted drop immediately while its queued write runs.
    pub fn apply_card_drop(&mut self, drag: CardDragState) {
        let Some(target_lane) = drag.hovered_lane else { return };
        if let Some(snapshot) = self.states.get_mut(&drag.workspace_id).and_then(state_snapshot_mut)
        {
            move_snapshot_card(snapshot, drag.source_lane, target_lane, &drag.source.id);
        }
        self.optimistic_drops.insert(
            (drag.workspace_id, drag.source.id),
            OptimisticDrop { source_lane: drag.source_lane, target_lane, generation: None },
        );
    }

    /// Tag a committed overlay or snap a failed write back to its source.
    pub fn finish_card_drop(
        &mut self,
        workspace_id: WorkspaceId,
        issue_id: &str,
        result: &BeadsIssueWriteResult,
    ) {
        let key = (workspace_id, issue_id.to_owned());
        if let BeadsIssueWriteResult::Applied { generation } = result {
            if let Some(drop) = self.optimistic_drops.get_mut(&key) {
                drop.generation = Some(*generation);
            }
            return;
        }
        self.cancel_card_drop(workspace_id, issue_id);
    }

    pub fn cancel_card_drop(&mut self, workspace_id: WorkspaceId, issue_id: &str) {
        let Some(drop) = self.optimistic_drops.remove(&(workspace_id, issue_id.to_owned())) else {
            return;
        };
        if let Some(snapshot) = self.states.get_mut(&workspace_id).and_then(state_snapshot_mut) {
            move_snapshot_card(snapshot, drop.target_lane, drop.source_lane, issue_id);
        }
    }

    /// The shortest board that still shows the headband with one whole row
    /// under it (A2-G1, A2-G4).
    fn min_height(&self) -> f32 {
        let scale = self.text_scale();
        beads_board_a2::HEADBAND_H
            + beads_board_a2::LANES_PADDING_BOTTOM
            + beads_board_a2::FLOOR_H
            + beads_board_a2::ROW_H * scale
    }

    /// Whether `workspace_id`'s board is pinned open.
    pub fn is_pinned(&self, workspace_id: WorkspaceId) -> bool {
        self.pinned.contains(&workspace_id)
    }

    /// Pin or unpin one region's board, leaving every other region alone.
    pub fn toggle_pin(&mut self, workspace_id: WorkspaceId) {
        if !self.pinned.remove(&workspace_id) {
            self.pinned.insert(workspace_id);
        }
    }

    /// Every pinned board, in a stable order so a caller comparing this
    /// against a persisted list sees a change only when one really happened.
    pub fn pinned(&self) -> Vec<WorkspaceId> {
        let mut pinned: Vec<WorkspaceId> = self.pinned.iter().copied().collect();
        pinned.sort_by_key(WorkspaceId::as_uuid);
        pinned
    }

    /// Pin `queue`'s collapsed lane open for `workspace_id`, replacing
    /// whichever queue was pinned before it — at most one of Blocked and Done
    /// is ever pinned. Rejects a queue that is not Blocked or Done, and is a
    /// no-op for a queue that is already the pinned one.
    ///
    /// Clears any transient hover/focus this queue was tracking: once pinned
    /// it is a full lane, not a tab, and a stale bit left behind could not be
    /// told apart from the pointer still resting on a tab that no longer
    /// exists.
    pub fn pin_lane(&mut self, workspace_id: WorkspaceId, queue: BeadsIssueQueue) -> bool {
        if !collapsible_queue(queue) || self.lane_pinned.get(&workspace_id) == Some(&queue) {
            return false;
        }
        self.lane_pinned.insert(workspace_id, queue);
        if self.lane_hovered.get(&workspace_id).is_some_and(|(tracked, _)| *tracked == queue) {
            self.lane_hovered.remove(&workspace_id);
            self.lane_hover_expires.remove(&workspace_id);
        }
        true
    }

    /// Unpin `queue`'s collapsed lane, restoring it to a plain tab. A no-op
    /// for a queue that is not the one currently pinned, including an
    /// invalid one.
    pub fn unpin_lane(&mut self, workspace_id: WorkspaceId, queue: BeadsIssueQueue) -> bool {
        if !collapsible_queue(queue) || self.lane_pinned.get(&workspace_id) != Some(&queue) {
            return false;
        }
        self.lane_pinned.remove(&workspace_id);
        true
    }

    /// What `queue`'s collapsed lane looks like right now. `None` for a queue
    /// that is not Blocked or Done — this state exists for only those two.
    /// Pinned wins over a simultaneous hover, since a pinned queue is a full
    /// lane rather than a tab with a drawer left to open.
    pub fn collapsed_lane_state(
        &self,
        workspace_id: WorkspaceId,
        queue: BeadsIssueQueue,
    ) -> Option<CollapsedLaneState> {
        if !collapsible_queue(queue) {
            return None;
        }
        if self.lane_pinned.get(&workspace_id) == Some(&queue) {
            return Some(CollapsedLaneState::Pinned);
        }
        let open =
            self.lane_hovered.get(&workspace_id).is_some_and(|(tracked, _)| *tracked == queue);
        Some(if open { CollapsedLaneState::Open } else { CollapsedLaneState::Tab })
    }

    /// Every pinned lane, in the same stable order `pinned()` gives the board
    /// pins, for a caller comparing this against a persisted record.
    pub fn lane_pinned(&self) -> Vec<(WorkspaceId, BeadsIssueQueue)> {
        let mut pinned: Vec<_> =
            self.lane_pinned.iter().map(|(workspace_id, queue)| (*workspace_id, *queue)).collect();
        pinned.sort_by_key(|(workspace_id, _)| workspace_id.as_uuid());
        pinned
    }

    /// Take the pins a previous run of this window left behind. They apply as
    /// each named region appears.
    pub fn restore_pins(&mut self, pinned: impl IntoIterator<Item = WorkspaceId>) {
        self.pending_pins.extend(pinned);
    }

    /// Take the lane pins a previous run of this window left behind — the
    /// lane-level twin of `restore_pins`. Rejects a queue that is not Blocked
    /// or Done rather than coercing it into one that is.
    pub fn restore_lane_pins(
        &mut self,
        pinned: impl IntoIterator<Item = (WorkspaceId, BeadsIssueQueue)>,
    ) {
        self.pending_lane_pins
            .extend(pinned.into_iter().filter(|(_, queue)| collapsible_queue(*queue)));
    }

    /// Drop every workspace this window no longer shows a region for.
    ///
    /// Reconciled against the live layout rather than hooked onto each path
    /// that can close a region: a leaked entry would keep re-requesting a board
    /// for a workspace nobody can see, every thirty seconds, forever.
    pub fn retain_regions(&mut self, live: &HashSet<WorkspaceId>) {
        for workspace_id in live {
            if self.pending_pins.remove(workspace_id) {
                self.pinned.insert(*workspace_id);
            }
            if let Some(queue) = self.pending_lane_pins.remove(workspace_id) {
                self.lane_pinned.insert(*workspace_id, queue);
            }
        }
        self.states.retain(|workspace_id, _| live.contains(workspace_id));
        self.retry_after.retain(|workspace_id, _| live.contains(workspace_id));
        self.hover_expires.retain(|workspace_id, _| live.contains(workspace_id));
        self.hovered.retain(|workspace_id, _| live.contains(workspace_id));
        self.pinned.retain(|workspace_id| live.contains(workspace_id));
        self.lane_pinned.retain(|workspace_id, _| live.contains(workspace_id));
        self.lane_hovered.retain(|workspace_id, _| live.contains(workspace_id));
        self.lane_hover_expires.retain(|workspace_id, _| live.contains(workspace_id));
        self.heights.retain(|workspace_id, _| live.contains(workspace_id));
        self.optimistic_drops.retain(|(workspace_id, _), _| live.contains(workspace_id));
        self.flows.retain(|workspace_id, _| live.contains(workspace_id));
        self.flow_open_order.retain(|workspace_id| live.contains(workspace_id));
        self.pending_flows.retain(|workspace_id, _| live.contains(workspace_id));
        self.flow_requests.retain(|(workspace_id, _)| live.contains(workspace_id));
        if self.card_press.as_ref().is_some_and(|press| !live.contains(&press.workspace_id))
            || self.card_drag.as_ref().is_some_and(|drag| !live.contains(&drag.workspace_id))
        {
            self.end_card_drag();
        }
    }

    /// Report the pointer entering or leaving one of the things that keep
    /// `workspace_id`'s board open.
    ///
    /// Sources are tracked separately because they overlap and report out of
    /// order: a control inside the board takes the hover from the board, which
    /// then reports a leave it never had. Only when the last source is gone
    /// does the board start closing, and even then a grace period covers the
    /// gap the pointer crosses on its way from the bead.
    pub fn hover(&mut self, workspace_id: WorkspaceId, source: HoverSource, hovered: bool) {
        let sources = self.hovered.entry(workspace_id).or_default();
        if hovered {
            *sources |= source as u8;
        } else {
            *sources &= !(source as u8);
        }
        if *sources == 0 {
            self.hover_expires.insert(workspace_id, Instant::now() + Duration::from_millis(150));
        } else {
            self.hover_expires.remove(&workspace_id);
        }
    }

    /// Report the pointer or keyboard focus entering or leaving one of the
    /// things that keep a collapsed lane's drawer open for `workspace_id`.
    ///
    /// A2-I1 opens at most one drawer per workspace, so entering a queue
    /// other than the one already tracked replaces it outright instead of
    /// tracking Blocked and Done independently — there is no grace period to
    /// hand the old one, because the two were never open together. A leave
    /// for a queue that is not the one tracked is stale and ignored, the same
    /// filter `set_flow_hover` applies to an out-of-order leave. Rejects a
    /// queue that is not Blocked or Done.
    pub fn hover_lane(
        &mut self,
        workspace_id: WorkspaceId,
        queue: BeadsIssueQueue,
        source: LaneHoverSource,
        entered: bool,
    ) -> bool {
        if !collapsible_queue(queue) {
            return false;
        }
        let tracked = self.lane_hovered.get(&workspace_id).copied();
        let next = match tracked {
            Some((current, sources)) if current == queue => {
                (queue, if entered { sources | source as u8 } else { sources & !(source as u8) })
            }
            _ if entered => (queue, source as u8),
            _ => return false,
        };
        if tracked == Some(next) {
            return false;
        }
        if next.1 == 0 {
            self.lane_hover_expires
                .insert(workspace_id, Instant::now() + Duration::from_millis(150));
        } else {
            self.lane_hover_expires.remove(&workspace_id);
        }
        self.lane_hovered.insert(workspace_id, next);
        true
    }

    /// Force `workspace_id`'s open transient drawer closed for `queue`, the
    /// way Escape does. A pinned lane is not a drawer, so Escape never
    /// touches it, and a queue that is not the one currently tracked (or not
    /// Blocked/Done at all) is a no-op.
    pub fn close_lane_drawer(&mut self, workspace_id: WorkspaceId, queue: BeadsIssueQueue) -> bool {
        if !collapsible_queue(queue) || self.lane_pinned.get(&workspace_id) == Some(&queue) {
            return false;
        }
        if !self.lane_hovered.get(&workspace_id).is_some_and(|(tracked, _)| *tracked == queue) {
            return false;
        }
        self.lane_hovered.remove(&workspace_id);
        self.lane_hover_expires.remove(&workspace_id);
        true
    }

    /// Close every workspace's open transient collapsed-lane drawer, the way
    /// a bare Escape does (specs/028's "Escape closes only a transient
    /// drawer" closed decision) when no more specific target -- a workspace,
    /// a queue -- is available to a window-wide keystroke. A pinned lane is
    /// never in `lane_hovered`, so this can never unpin one. Returns whether
    /// anything closed.
    pub fn close_any_lane_drawer(&mut self) -> bool {
        let open: Vec<(WorkspaceId, BeadsIssueQueue)> = self
            .lane_hovered
            .iter()
            .map(|(workspace_id, (queue, _))| (*workspace_id, *queue))
            .collect();
        let mut closed = false;
        for (workspace_id, queue) in open {
            closed |= self.close_lane_drawer(workspace_id, queue);
        }
        closed
    }

    pub fn expire_hover(&mut self) -> bool {
        let now = Instant::now();
        let held_resize = self.resizing();
        let held_drag = self.card_drag.as_ref().map(|drag| drag.workspace_id);
        let due: Vec<WorkspaceId> = self
            .hover_expires
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(workspace_id, _)| *workspace_id)
            // A drag takes the pointer off its board; closing that board
            // mid-gesture would discard either resize or card state.
            .filter(|workspace_id| {
                held_resize != Some(*workspace_id) && held_drag != Some(*workspace_id)
            })
            .collect();
        for workspace_id in &due {
            self.hovered.remove(workspace_id);
            self.hover_expires.remove(workspace_id);
        }
        let lane_due: Vec<WorkspaceId> = self
            .lane_hover_expires
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(workspace_id, _)| *workspace_id)
            .collect();
        for workspace_id in &lane_due {
            self.lane_hovered.remove(workspace_id);
            self.lane_hover_expires.remove(workspace_id);
        }
        !due.is_empty() || !lane_due.is_empty()
    }
}

impl CardDragGhost {
    fn new(source: BeadsBoardItem, board: Rect, scale: f32) -> Self {
        let lane_width = (board.width - 2.0 * LANES_SIDE_PADDING) / 5.0;
        Self {
            source,
            width: (lane_width - 2.0 * LANE_CARD_PADDING - LANE_BODY_RIGHT_PADDING).max(1.0),
            height: (ISSUE_HEIGHT - CARD_GAP) * scale,
        }
    }
}

fn card_drag_source(lane: u8) -> bool {
    lane <= 2
}

/// Whether `queue` is one of the two queues A2-I1/A2-I2 collapse to a rail
/// tab. Backlog, Ready, and In progress are rejected rather than coerced into
/// one of these two, the same way `card_drag_source` gates which lanes can
/// arm a drag.
fn collapsible_queue(queue: BeadsIssueQueue) -> bool {
    matches!(queue, BeadsIssueQueue::Blocked | BeadsIssueQueue::Done)
}

fn state_snapshot_mut(state: &mut BeadsBoardState) -> Option<&mut BeadsBoardSnapshot> {
    match state {
        BeadsBoardState::Loading { cached } => cached.as_mut(),
        BeadsBoardState::Ready { snapshot, .. } => Some(snapshot),
        BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => None,
    }
}

fn snapshot_card_lane(snapshot: &BeadsBoardSnapshot, issue_id: &str) -> Option<u8> {
    [&snapshot.backlog, &snapshot.ready, &snapshot.in_progress, &snapshot.blocked, &snapshot.done]
        .into_iter()
        .position(|cards| cards.iter().any(|card| card.id == issue_id))
        .and_then(|lane| u8::try_from(lane).ok())
}

fn lane_cards_mut(snapshot: &mut BeadsBoardSnapshot, lane: u8) -> Option<&mut Vec<BeadsBoardItem>> {
    match lane {
        0 => Some(&mut snapshot.backlog),
        1 => Some(&mut snapshot.ready),
        2 => Some(&mut snapshot.in_progress),
        3 => Some(&mut snapshot.blocked),
        4 => Some(&mut snapshot.done),
        _ => None,
    }
}

fn lane_total_mut(snapshot: &mut BeadsBoardSnapshot, lane: u8) -> Option<&mut u32> {
    match lane {
        0 => Some(&mut snapshot.backlog_total),
        1 => Some(&mut snapshot.ready_total),
        2 => Some(&mut snapshot.in_progress_total),
        3 => Some(&mut snapshot.blocked_total),
        4 => Some(&mut snapshot.done_total),
        _ => None,
    }
}

fn move_snapshot_card(
    snapshot: &mut BeadsBoardSnapshot,
    source_lane: u8,
    target_lane: u8,
    issue_id: &str,
) -> bool {
    if source_lane > 4 || target_lane > 4 {
        return false;
    }
    let Some(index) = lane_cards_mut(snapshot, source_lane)
        .and_then(|cards| cards.iter().position(|card| card.id == issue_id))
    else {
        return false;
    };
    let Some(card) = lane_cards_mut(snapshot, source_lane).map(|cards| cards.remove(index)) else {
        return false;
    };
    let Some(source_total) = lane_total_mut(snapshot, source_lane) else { return false };
    *source_total = source_total.saturating_sub(1);
    let Some(target_total) = lane_total_mut(snapshot, target_lane) else { return false };
    *target_total = target_total.saturating_add(1);
    lane_cards_mut(snapshot, target_lane).is_some_and(|cards| {
        cards.push(card);
        true
    })
}

fn card_drag_lane(board: Rect, pointer: CardDragPoint) -> Option<u8> {
    let left = board.x + LANES_SIDE_PADDING;
    let right = board.x + board.width - LANES_SIDE_PADDING;
    if pointer.x < left
        || pointer.x >= right
        || pointer.y < board.y
        || pointer.y >= board.y + board.height
    {
        return None;
    }
    let lane_width = (right - left) / 5.0;
    let x = pointer.x - left;
    Some(if x < lane_width {
        0
    } else if x < lane_width * 2.0 {
        1
    } else if x < lane_width * 3.0 {
        2
    } else if x < lane_width * 4.0 {
        3
    } else {
        4
    })
}

fn card_drag_point(position: Point<Pixels>) -> CardDragPoint {
    CardDragPoint { x: f32::from(position.x), y: f32::from(position.y) }
}

/// Height a board opens at, shared by the paint and by the region reservation
/// a pinned board makes, so the two cannot disagree. A drag of the bottom bar
/// moves it for that one workspace.
pub const BEADS_BOARD_HEIGHT: f32 = 197.0;

/// How far either side of the bottom bar counts as grabbing it, matching the
/// tolerance a pane divider gives its own one-pixel line.
pub const BEADS_BOARD_GRIP: f32 = 4.0;

/// The board's palette, derived from the live theme.
///
/// The board takes its structure, weights and alphas from
/// `.impeccable/mocks/beads-compact-live-overview.html`, but the mock's fixed
/// colours are read off the theme instead so a board belongs to whatever
/// palette the terminal is wearing. Queue states take the ANSI colours their
/// meaning already implies — ready cyan, in progress blue, blocked red, done
/// green — which every theme defines.
///
/// Where the mock lays its issues on the bare ground, an issue here is a raised
/// card, so the palette carries two grounds: the strip's, which the lane heads
/// sit on, and the card's, which every word inside an issue sits on.
#[derive(Debug, Clone, Copy)]
pub struct BeadsBoardColors {
    /// The strip's ground, already composited with the window's opacity.
    pub ground: Rgba,
    /// A card's fill, lit from the top: the pair are the two ends of its
    /// gradient, and the second is the flat colour the card reads as.
    pub card_top: Rgba,
    pub card: Rgba,
    pub card_hover_top: Rgba,
    pub card_hover: Rgba,
    pub card_border: Rgba,
    pub card_border_hover: Rgba,
    pub title: Rgba,
    pub queue_name: Rgba,
    pub queue_name_active: Rgba,
    pub muted: Rgba,
    /// A2's darkest named text role (A2-C1): age, void copy, and an empty
    /// tab's count sit here, one step below `muted`.
    pub quiet: Rgba,
    pub hairline: Rgba,
    pub chevron: Rgba,
    pub button_hover: Rgba,
    /// The A2 floor's own wash and its centred resize grip (A2-C8):
    /// distinct from `band`'s "subtle lift" because the grip has to read a
    /// step brighter than the floor it sits on.
    pub grip: Rgba,
    pub epic: Rgba,
    pub backlog_state: Rgba,
    pub ready_state: Rgba,
    pub progress_state: Rgba,
    pub blocked_state: Rgba,
    pub done_state: Rgba,
    /// P0 through P4, hottest first.
    pub priorities: [Rgba; 5],
    /// A wire between two nodes in the Flow view: structure rather than a
    /// mark, so it stays faint enough that a dense graph reads as nodes with
    /// connections instead of as a wiring diagram.
    pub wire: Rgba,
    /// A wire on the hovered node's path, which is the one the reader is
    /// actually following, so it carries the title's own ink.
    pub wire_traced: Rgba,
    /// A wire off that path. Deliberately below any floor: being hard to read
    /// is what the dimmed state is for.
    pub wire_dimmed: Rgba,
    /// The Flow band's fill, above the graph.
    pub band: Rgba,
    /// The unfilled part of the band's progress bar.
    pub progress_track: Rgba,
    /// The opened node's fill and the keyline down its leading edge.
    pub cursor_fill: Rgba,
    pub cursor_keyline: Rgba,
    /// The floating count chip a traced node raises, and its hairline.
    pub chip: Rgba,
    pub chip_border: Rgba,
    /// The rank ruler's labels.
    pub rank_label: Rgba,
    /// The agent line on a node a session is live on. Distinct from
    /// `progress_state`, which the live dot itself keeps.
    pub agent: Rgba,
    /// The ring around a live node's dot. The mock composites the progress
    /// hue at 20% over whatever the node sits on, so it is a real alpha
    /// rather than a tint into the ground: a live node can also be the
    /// cursor, and a ring mixed into the ground would erase that fill.
    pub agent_halo: Rgba,
}

impl BeadsBoardColors {
    /// Derive the board's palette from the chrome slots and the ANSI ramp.
    #[must_use]
    pub fn from_theme(
        chrome: &scribe_common::theme::ChromeColors,
        ansi: &[[f32; 4]; 16],
        opacity: f32,
    ) -> Self {
        let ground = surface(chrome.tab_bar_bg, opacity);
        // Read against the ground as it is actually seen: the strip may be
        // translucent, but the text sits on what the eye reads as its ground.
        let ink = Rgba { a: 1.0, ..ground };
        // Elevation is light, in every theme: a raised card is the ground
        // carried toward white, which on a pale theme lands on the white a
        // paper card would be and leaves its border and its shadow to carry
        // the lift.
        let card = mix(ink, WHITE, 0.055);
        let card_hover = mix(ink, WHITE, 0.105);
        // A border has to darken on a pale theme and lighten on a dark one, so
        // it moves toward whichever end the ground is not.
        let border_target = if luminance(ink) < 0.5 { WHITE } else { BLACK };
        // Text clears a contrast floor against the ground it is read on, and
        // the two grounds pull opposite ways: the card is lighter, which is
        // the worse case for a dark theme's pale ink and the better one for a
        // pale theme's dark ink. So every word is measured on both. The mock's
        // tones are relative to the mock's own ground, and a theme whose muted
        // slot or ANSI red is close to its background reproduces the ratio,
        // not the legibility: alpha-reducing an already-dim slot is what made
        // blockers and ids unreadable.
        let anywhere = |color: Rgba| readable_anywhere(color, ink, card, BODY_CONTRAST);
        let text = anywhere(slot(chrome.tab_text_active));
        let muted = anywhere(slot(chrome.tab_text));
        let hairline = slot(chrome.tab_separator);
        let blocked = anywhere(slot(ansi[BRIGHT_RED]));
        // P0 takes the more saturated of the theme's two reds, and P1 is that
        // same red pulled toward the neutral. Derived from each other rather
        // than from a slot each, because lifting a colour to clear the floor
        // washes it out, and two slots lifted by different amounts can come
        // out ranked either way round.
        // A heat scale, because that is what a priority is: red, then amber,
        // then yellow, then out. Every step is a different hue rather than the
        // same red at three strengths, which is what a reader has to tell
        // apart at a glance and across a wash this faint.
        let vivid = |plain: Rgba, bright: Rgba| {
            if vividness(plain) >= vividness(bright) { plain } else { bright }
        };
        let critical = vivid(slot(ansi[RED]), slot(ansi[BRIGHT_RED]));
        let caution = vivid(slot(ansi[YELLOW]), slot(ansi[BRIGHT_YELLOW]));
        // No terminal palette carries an amber, so it is mixed from the two it
        // sits between, which keeps it in this theme's own reds and yellows.
        let high = mix(critical, caution, 0.45);
        let progress = slot(ansi[BRIGHT_BLUE]);
        let epic = anywhere(mix(slot(ansi[BRIGHT_MAGENTA]), muted, 0.45));
        // Flow lays its surfaces on the strip as the mock's white-alpha
        // overlays, which only read as a lift on a dark theme. A tint here
        // therefore travels toward whichever end the ground is not, the same
        // way a card border does, or a pale theme paints white on white. The
        // amounts are the mock's own alphas: a colour composited at alpha `a`
        // over the ground is that ground mixed `a` of the way to it.
        let tint = |amount: f32| mix(ink, border_target, amount);
        // Lifted here rather than inline because the live halo is this exact
        // mark thinned: deriving the ring from the raw ANSI slot would leave
        // it a different hue from the dot it surrounds on any theme whose
        // blue needs the lift.
        let progress_state = readable(progress, ink, MARK_CONTRAST);
        Self {
            ground,
            card_top: mix(card, WHITE, 0.03),
            card,
            card_hover_top: mix(card_hover, WHITE, 0.03),
            card_hover,
            card_border: mix(card, border_target, 0.1),
            card_border_hover: mix(card, border_target, 0.22),
            title: text,
            // The mock keeps three steps of brightness across the strip: the
            // issue title, a queue name, and the lane the eye should land on.
            // The steps are taken between the floor and the ground, never
            // below the floor.
            queue_name: mix(text, muted, 0.35),
            queue_name_active: text,
            muted,
            // A2's fourth, darkest step: age, void copy, and an empty
            // tab's own count sit below `muted` the same way `muted` sits
            // below `queue_name` (A2-C1, A2-C4).
            quiet: anywhere(mix(muted, ground, 0.3)),
            hairline,
            // Marks rather than text, so they clear the lower floor a
            // non-text element needs.
            chevron: readable(muted, ink, MARK_CONTRAST),
            button_hover: alpha(text, 0.08),
            // The floor's own resize grip has to read a step brighter than
            // the floor wash it sits on, so it takes a stronger lift than
            // `band`'s "subtle lift" while staying in the same family
            // (A2-C8).
            grip: tint(0.17),
            // An epic is a grouping label, not another muted field: it
            // takes the one ANSI hue no queue or priority has claimed, pulled
            // most of the way to muted so it stays quiet beside the id.
            epic,
            backlog_state: readable(muted, ink, MARK_CONTRAST),
            ready_state: readable(slot(ansi[BRIGHT_CYAN]), ink, MARK_CONTRAST),
            progress_state,
            blocked_state: readable(blocked, ink, MARK_CONTRAST),
            done_state: readable(slot(ansi[BRIGHT_GREEN]), ink, MARK_CONTRAST),
            priorities: [
                // A theme carries two of each hue and either can be the pale
                // one, so each step takes the more saturated: a washed-out
                // pink says less than a deep red however light it is, and the
                // ranking cannot be left to which slot a theme happened to
                // fill.
                anywhere(critical),
                anywhere(high),
                anywhere(caution),
                // Off the scale: the last two carry no heat, so they carry no
                // hue either.
                muted,
                // Stepped toward the ground for the hierarchy, then lifted
                // back if that step took it through the floor.
                anywhere(mix(muted, ground, 0.25)),
            ],
            wire: tint(0.22),
            // A traced wire and the cursor's keyline are the two marks that
            // say which run the reader is on, so both take the title's ink
            // rather than a tint: they are the only things in the graph that
            // have to win against everything drawn near them.
            wire_traced: text,
            wire_dimmed: tint(0.078),
            band: tint(0.035),
            progress_track: tint(0.078),
            cursor_fill: tint(0.059),
            cursor_keyline: text,
            // A chip floats above the graph the way a card sits above the
            // strip, so it is the same raised surface with a stronger edge —
            // it has wires running under it and needs to cut them.
            chip: card,
            chip_border: mix(card, border_target, 0.169),
            // The ruler names ranks, not issues, so it steps further back than
            // any card field before the floor lifts it again.
            rank_label: anywhere(mix(muted, ground, 0.42)),
            // Liveness is in-progress-ness observed rather than a separate
            // meaning, so the agent line stays in the progress hue's family
            // instead of taking a hue of its own. Carrying it toward the title
            // is what keeps it apart from the dot it annotates.
            agent: anywhere(mix(progress, text, 0.28)),
            // The dot itself stays the progress hue, so the ring is that same
            // hue thinned rather than a second colour: the halo reads as the
            // dot's own glow instead of as another mark beside it.
            agent_halo: alpha(progress_state, 0.2),
        }
    }

    /// Lift a queue state from mark contrast to body-text contrast on a panel.
    pub(crate) fn panel_state_ink(&self, state: Rgba) -> Rgba {
        readable(state, self.card, BODY_CONTRAST)
    }

    fn priority(&self, priority: u8) -> Rgba {
        self.priorities.get(usize::from(priority)).copied().unwrap_or(self.muted)
    }

    /// The wash behind a priority and the ink that stays readable on it.
    ///
    /// The wash weakens as the rank falls, so the hierarchy survives a theme
    /// whose reds are close: P0 is both the most saturated colour on the card
    /// and the strongest mark, and neither depends on the other.
    fn priority_mark(&self, priority: u8) -> PriorityMark {
        let color = self.priority(priority);
        let weight = PRIORITY_WEIGHTS
            .get(usize::from(priority))
            .copied()
            .unwrap_or(PRIORITY_FAINTEST_WEIGHT);
        let (floor, ceiling) = PRIORITY_TINT_RANGE;
        let tint = (weight / reach(color, self.card).max(0.02)).clamp(floor, ceiling);
        let fill = mix(self.card, color, (tint * BADGE_OF_WASH).clamp(0.2, 1.0));
        // A filled badge is read the other way round from everything else on
        // the card: the colour is the ground, so the digits take whichever end
        // of the range that ground is not.
        let on_fill = if luminance(fill) < 0.5 { WHITE } else { BLACK };
        PriorityMark { ink: readable(on_fill, fill, BODY_CONTRAST), fill }
    }
}

/// One priority's mark: the wash laid behind it, and the ink for its digits.
#[derive(Clone, Copy)]
struct PriorityMark {
    /// The digits' colour, which is read on the badge rather than on the card.
    ink: Rgba,
    /// The badge's own fill, laid over the card at a strength solved from the
    /// rank, so a hue near the card reads as strongly as one far from it.
    fill: Rgba,
}

/// How far a colour sits from a ground, averaged over the channels: how much
/// of itself a wash of it lays down.
fn reach(color: Rgba, ground: Rgba) -> f32 {
    ((color.r - ground.r).abs() + (color.g - ground.g).abs() + (color.b - ground.b).abs()) / 3.0
}

/// How saturated a colour is, which is what makes it read as urgent: a washed
/// out pink says less than a deep red however light it is.
fn vividness(color: Rgba) -> f32 {
    let high = color.r.max(color.g).max(color.b);
    let low = color.r.min(color.g).min(color.b);
    if high <= 0.0 { 0.0 } else { (high - low) / high }
}

/// Contrast a colour carrying words must reach against the board's ground,
/// and the lower one a dot, a chevron, or a hairline-thin mark needs. The text
/// floor is WCAG AA for body text; marks carry no reading load, so holding
/// them to it would only wash the queue colours out.
pub(crate) const BODY_CONTRAST: f32 = 4.5;
const MARK_CONTRAST: f32 = 3.0;

const RED: usize = 1;
const YELLOW: usize = 3;
const BRIGHT_RED: usize = 9;
const BRIGHT_GREEN: usize = 10;
const BRIGHT_YELLOW: usize = 11;
const BRIGHT_MAGENTA: usize = 13;
const BRIGHT_BLUE: usize = 12;
const BRIGHT_CYAN: usize = 14;

const WHITE: Rgba = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

/// The drag ghost's own geometry (`CardDragGhost::new`, `card_drag_lane`):
/// the raised-card sizing scribe-zwtv.9 still owns until it redraws the
/// ghost and drop targets for A2's unequal tracks.
const LANES_SIDE_PADDING: f32 = 8.0;
const LANE_CARD_PADDING: f32 = 8.0;
const LANE_BODY_RIGHT_PADDING: f32 = 4.0;
const ISSUE_HEIGHT: f32 = 50.0;
const CARD_GAP: f32 = 4.0;
const CARD_RADIUS: f32 = 4.0;

/// The borderless text-size steppers' own left-gutter geometry (A2-G2),
/// mirrored from `a2a3-contract.json`'s `geometry.a2.zoom_*` block.
const ZOOM_LEFT: f32 = 8.0;
const ZOOM_TOP: f32 = 5.0;
const ZOOM_GLYPH_W: f32 = 12.0;
const ZOOM_GLYPH_H: f32 = 17.0;
const ZOOM_GAP: f32 = 1.0;

/// A lane's overflow cue (A2-G9: "Overflow chevron is 10px at right
/// 1px/bottom 0").
const CHEV_SIZE: f32 = 10.0;
const CHEV_RIGHT: f32 = 1.0;
const CHEV_BOTTOM: f32 = 0.0;

/// A collapsed tab's own `‹` cue (not the lane overflow `‹CHEV_*` marks
/// above): 11x11px, a 7px gap off the tab's bottom edge, 2px corner radius
/// for the chip its hot background paints. Not part of the generated machine
/// contract, the same way `PINNED_LANE_SHARE` in `beads_board_a2` is not:
/// `gen-contract.py` extracts no `.dr .tab .cue` box geometry, only the
/// structural CSS it already asserts against.
const TAB_CUE_SIZE: f32 = 11.0;
const TAB_CUE_RADIUS: f32 = 2.0;
const TAB_CUE_MARGIN_BOTTOM: f32 = 7.0;

/// The strip's own bottom resize grip (A2-G9: "Floor is 3px with a centred
/// 34×1px grip at top 1px").
const FLOOR_GRIP_W: f32 = 34.0;
const FLOOR_GRIP_H: f32 = 1.0;
const FLOOR_GRIP_TOP: f32 = 1.0;

/// Where a row's sub line indents to under the title, and where the void
/// copy that replaces an empty lane's rows starts too (A2-S5's
/// title-aligned void copy): the priority column plus its gap to the title.
const ROW_TITLE_INDENT: f32 = beads_board_a2::ROW_PRIORITY_W + beads_board_a2::ROW_PRIORITY_GAP;

/// How much colour each rank's badge carries, as a mean channel distance from
/// the card it sits on. P0 through P4.
///
/// The tint is solved for this rather than fixed, because a hue far from the
/// card needs less of itself to make the same mark than one that sits near it:
/// at one fixed tint a vivid yellow at P2 out-shouts a dulled red at P1, which
/// is a ranking the reader can see and the ramp did not intend.
const PRIORITY_WEIGHTS: [f32; 5] = [0.105, 0.078, 0.058, 0.042, 0.032];
const PRIORITY_FAINTEST_WEIGHT: f32 = 0.032;
/// What a solved tint may not go past: a colour sitting almost on the card
/// would otherwise be asked for a wash stronger than itself.
const PRIORITY_TINT_RANGE: (f32, f32) = (0.04, 0.5);
/// How much more of a colour a badge carries than a broad wash of the same
/// rank. Scaled so the hottest rank lands on the colour itself and the coolest
/// keeps a fill it can still be read against.
const BADGE_OF_WASH: f32 = 3.0;
/// The card's own top padding.
const CARD_PAD_TOP: f32 = 6.0;
/// The title's line box, shared by the badge beside it so both sit on one
/// line.
const TITLE_LINE: f32 = 17.0;

const TEXT_SCALE_STEP: f32 = 0.1;
const MIN_TEXT_SCALE_STEPS: i8 = -2;
const MAX_TEXT_SCALE_STEPS: i8 = 6;

/// The text scale every board word and the drag ghost paint at.
///
/// A2's own row height and track widths come straight from
/// [`beads_board_a2::layout`] instead: only the repeating row unit scales
/// there (A2-G1, A2-G10), while the structural rail geometry this type still
/// covers -- the ghost's card sizing, and the handful of small row-internal
/// paddings paint code scales for legibility -- stays a plain factor on one
/// designed pixel value.
#[derive(Clone, Copy)]
struct Metrics {
    scale: f32,
}

impl Metrics {
    fn at(self, designed: f32) -> gpui::Pixels {
        px(designed * self.scale)
    }
}

pub struct BeadsBoardRender {
    /// The strip at the top of this workspace's region, in grid-band
    /// coordinates. The board is a region citizen, never a window-wide band:
    /// a window showing two regions side by side must keep each board over the
    /// terminal it describes.
    pub rect: Rect,
    /// Hovered boards float over the panes; a pinned board fills space its
    /// region already reserved for it.
    pub overlay: bool,
    pub hover_state: std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    pub panel_state: std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    pub workspace_id: WorkspaceId,
    /// Hovered lane copied from the caller's existing board-store guard.
    pub drag_target: Option<u8>,
    /// Blocked and Done's collapsed-lane state, read under the caller's
    /// existing board-store guard: whether the A2 layout gives each a
    /// pinned lane's track or a plain rail tab (A2-S1, A2-S3).
    pub rail: RailState,
    /// Stable Tab stops for Blocked's and Done's own collapsible-lane
    /// control, held by the view across frames the same way
    /// [`FlowNodeControl::focus`] is: a fresh handle every render would
    /// reset keyboard focus and Tab order on every repaint. One handle per
    /// queue serves the tab, its hover/focus-open drawer, *and* the pinned
    /// lane's own unpin control, since those are one control across three
    /// paint states rather than three controls (A2-I2's "activating the
    /// pinned tab or its `×` control unpins it").
    pub blocked_tab_focus: FocusHandle,
    pub done_tab_focus: FocusHandle,
    /// Text scale shared by every board in this window.
    pub scale: f32,
    /// The live theme's board palette.
    pub colors: BeadsBoardColors,
    /// Focus and activation for every node of this workspace's Flow graph.
    ///
    /// Held by the view across frames so a node keeps its Tab stop, and empty
    /// whenever the board is painting lanes.
    pub flow_controls: HashMap<String, FlowNodeControl>,
    /// This workspace's Flow graph, already read out of the board store, or
    /// `None` when the strip is painting lanes.
    ///
    /// Read by the caller under the guard it already holds rather than looked
    /// up here: painting runs inside that guard, so a second lock on the same
    /// non-reentrant mutex would deadlock the board rather than fail.
    pub flow: Option<FlowStripSnapshot>,
}

/// Shared stores lane and row builders need after the snapshot borrow ends.
#[derive(Clone, Copy)]
struct BoardStores<'a> {
    workspace_id: WorkspaceId,
    boards: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    panels: &'a std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    rect: Rect,
    drag_target: Option<u8>,
}

/// Marker registered with GPUI's native drag arm for eligible Beads cards.
struct BeadsCardDrag;

/// The native drag payload. GPUI paints this entity as a window root after
/// normal and deferred content, so it follows the pointer outside lane clips.
struct BeadsCardDragGhost {
    model: CardDragGhost,
    colors: BeadsBoardColors,
    metrics: Metrics,
}

/// Full issue title anchored above the normal card that owns the hover.
struct BeadsCardTooltip {
    title: String,
    anchor: Bounds<Pixels>,
    colors: BeadsBoardColors,
    text_size: Pixels,
}

impl Render for BeadsCardTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        anchored()
            .anchor(Anchor::BottomCenter)
            .position(point(self.anchor.center().x, self.anchor.origin.y - px(4.0)))
            .snap_to_window_with_margin(px(4.0))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .max_w(px(480.0))
                    .bg(alpha(self.colors.ground, 1.0))
                    .border_1()
                    .border_color(alpha(self.colors.title, 0.28))
                    .font_family("monospace")
                    .text_size(self.text_size)
                    .text_color(self.colors.title)
                    .child(self.title.clone()),
            )
    }
}

impl Render for BeadsCardDragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let item = &self.model.source;
        let mark = self.colors.priority_mark(item.priority);
        div()
            .w(px(self.model.width))
            .h(px(self.model.height))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .overflow_hidden()
            .rounded(px(CARD_RADIUS))
            .border_1()
            .border_color(self.colors.card_border_hover)
            .bg(linear_gradient(
                180.0,
                linear_color_stop(self.colors.card_hover_top, 0.0),
                linear_color_stop(self.colors.card_hover, 1.0),
            ))
            .shadow_lg()
            .pt(px(CARD_PAD_TOP))
            .px(px(8.0))
            .pb(px(6.0))
            .child(issue_title(item, mark, &self.colors, self.metrics))
            .child(drag_ghost_meta(item, &self.colors, self.metrics))
    }
}

/// Paint the compact live overview over the top of its own region.
pub fn render(
    workspace_name: &str,
    state: Option<&BeadsBoardState>,
    wiring: BeadsBoardRender,
) -> AnyElement {
    let BeadsBoardRender {
        rect,
        overlay,
        hover_state,
        panel_state,
        workspace_id,
        drag_target,
        rail,
        blocked_tab_focus,
        done_tab_focus,
        scale,
        colors,
        flow_controls,
        flow,
    } = wiring;
    let colors = &colors;
    let metrics = Metrics { scale };
    let (snapshot, status) = board_content(state);
    let board = board_shell(workspace_name, workspace_id, rect, colors, &hover_state);
    // Flow replaces the lanes inside the same strip: the reservation, the
    // resize grip and the text-size controls all stay where the board put
    // them, so returning to lanes cannot move the furniture around it.
    if let Some(strip) = flow_strip(FlowStrip {
        wheel_state: &hover_state,
        flow,
        workspace_id,
        rect,
        scale,
        colors,
        controls: &flow_controls,
    }) {
        return lift(board.child(strip).child(floor(colors)), overlay);
    }
    let board = match snapshot {
        Some(snapshot) => board.child(lanes(
            snapshot,
            BoardStores {
                workspace_id,
                boards: &hover_state,
                panels: &panel_state,
                rect,
                drag_target,
            },
            colors,
            metrics,
            RailFocus {
                rail,
                blocked_tab_focus: &blocked_tab_focus,
                done_tab_focus: &done_tab_focus,
            },
        )),
        // The mock draws no empty, loading, or unavailable state, so those keep
        // the one line of copy the board has always shown for them.
        None => board.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(metrics.at(12.0))
                .text_color(colors.muted)
                .child(status),
        ),
    };
    lift(board.child(floor(colors)), overlay)
}

/// The board's own root: positioned over the strip the region gave it --
/// painting to that rect rather than to a height of its own is what keeps a
/// dragged board from hanging past its own terminal -- with the headband and
/// text-size steppers already on it, since both stay put whether the strip
/// goes on to paint lanes or Flow.
fn board_shell(
    workspace_name: &str,
    workspace_id: WorkspaceId,
    rect: Rect,
    colors: &BeadsBoardColors,
    hover_state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
) -> gpui::Stateful<gpui::Div> {
    let drag_move = std::sync::Arc::clone(hover_state);
    div()
        .id(SharedString::from(format!("beads-board-{workspace_id}")))
        .aria_label(format!("{workspace_name} Beads overview"))
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.width))
        .h(px(rect.height))
        .flex()
        .flex_col()
        .bg(colors.ground)
        .border_b_1()
        .border_color(colors.hairline)
        .on_drag_move(move |event: &DragMoveEvent<BeadsCardDrag>, window, _app| {
            if let Ok(mut boards) = drag_move.lock()
                && boards.update_card_drag(card_drag_point(event.event.position), rect)
            {
                window.refresh();
            }
        })
        .on_hover({
            let hover_state = std::sync::Arc::clone(hover_state);
            move |hovered: &bool, _window, _app| {
                if let Ok(mut boards) = hover_state.lock() {
                    boards.hover(workspace_id, HoverSource::Board, *hovered);
                }
            }
        })
        // One hairline across the whole strip groups the five lane heads into
        // a single row instead of five floating labels (A2-G3); each lane's
        // own seam beneath it then carries that boundary's queue hue.
        .child(headband(colors))
        .child(text_size_controls(hover_state, workspace_id, colors))
}

/// The hairline that groups the five lane heads into one row (A2-G3).
fn headband(colors: &BeadsBoardColors) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(beads_board_a2::HEADBAND_H))
        .h(px(1.0))
        .bg(colors.hairline)
        .into_any_element()
}

/// The strip's own bottom resize grip: a 3px floor with a centred 34×1px
/// grip mark (A2-G9).
fn floor(colors: &BeadsBoardColors) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(beads_board_a2::FLOOR_H))
        .flex()
        .justify_center()
        .bg(colors.band)
        .child(
            div()
                .mt(px(FLOOR_GRIP_TOP))
                .flex_none()
                .w(px(FLOOR_GRIP_W))
                .h(px(FLOOR_GRIP_H))
                .bg(colors.grip),
        )
        .into_any_element()
}

/// A hovered board floats over live panes and needs the lift to read as
/// separate; a pinned one sits in space the region gave up for it.
fn lift<E: Styled + IntoElement>(board: E, overlay: bool) -> AnyElement {
    if overlay { board.shadow_lg().into_any_element() } else { board.into_any_element() }
}

/// Paint the Flow strip when this workspace is in Flow, else nothing.
///
/// The wheel is claimed here rather than on the board root because only a
/// Flow strip has an axis to move: in lanes the same gesture belongs to the
/// lane bodies underneath.
fn flow_strip(strip: FlowStrip<'_>) -> Option<AnyElement> {
    let FlowStrip { wheel_state, flow, workspace_id, rect, scale, colors, controls } = strip;
    let FlowStripSnapshot { graph, layout, cursor_issue_id, scroll_x, trace, live_issue_ids } =
        flow?;
    let painted = crate::beads_flow::render(&FlowRender {
        rect,
        graph: &graph,
        layout: &layout,
        cursor_issue_id: &cursor_issue_id,
        scroll_x,
        text_scale: scale,
        colors: *colors,
        node_controls: controls,
        trace: trace.as_ref(),
        live_issue_ids: &live_issue_ids,
    });
    let painted = match painted {
        Ok(painted) => painted,
        Err(error) => {
            tracing::debug!(%error, %workspace_id, "Beads Flow strip dropped");
            return None;
        }
    };
    let wheel_state = std::sync::Arc::clone(wheel_state);
    Some(
        div()
            .flex_1()
            .relative()
            .overflow_hidden()
            .on_scroll_wheel(move |event, window, app| {
                let delta = event.delta.pixel_delta(px(FLOW_WHEEL_LINE));
                // Either axis drives the one axis Flow has, so a plain
                // vertical wheel still travels the graph.
                let travel = if delta.x.abs() > delta.y.abs() {
                    -f32::from(delta.x)
                } else {
                    -f32::from(delta.y)
                };
                if let Ok(mut boards) = wheel_state.lock()
                    && boards.scroll_flow(workspace_id, travel, rect)
                {
                    window.refresh();
                    app.stop_propagation();
                }
            })
            .child(painted)
            .into_any_element(),
    )
}

/// One wheel line in pixels, used to turn a line-wise wheel into travel.
const FLOW_WHEEL_LINE: f32 = 20.0;

/// Everything the Flow strip needs from the board around it.
struct FlowStrip<'a> {
    /// Wheel target only. Event closures run after the caller's guard is
    /// dropped, so locking there is safe; locking during paint is not.
    wheel_state: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    /// `None` paints lanes. Owned rather than looked up here — see
    /// [`BeadsBoards::flow_snapshot`].
    flow: Option<FlowStripSnapshot>,
    workspace_id: WorkspaceId,
    rect: Rect,
    scale: f32,
    colors: &'a BeadsBoardColors,
    controls: &'a HashMap<String, FlowNodeControl>,
}

/// Everything painting a Flow strip needs, copied out of the board store.
///
/// The renderer is handed this instead of the store because `render` is
/// called while the caller still holds the board guard. Owned data is what
/// keeps the paint path off a mutex it cannot re-enter.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowStripSnapshot {
    pub graph: BeadsEpicGraph,
    pub layout: FlowLayout,
    pub cursor_issue_id: String,
    pub scroll_x: f32,
    pub trace: Option<FlowTrace>,
    pub live_issue_ids: HashSet<String>,
}

/// The board's own text-size control, in the strip's left gutter (A2-G2):
/// two borderless glyphs on the head's own line rather than two boxed
/// buttons in the top right corner.
fn text_size_controls(
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    workspace_id: WorkspaceId,
    colors: &BeadsBoardColors,
) -> AnyElement {
    div()
        .absolute()
        .left(px(ZOOM_LEFT))
        .top(px(ZOOM_TOP))
        .flex()
        .gap(px(ZOOM_GAP))
        .child(scale_button(state, workspace_id, colors, ScaleStep::Larger))
        .child(scale_button(state, workspace_id, colors, ScaleStep::Smaller))
        .into_any_element()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScaleStep {
    Smaller,
    Larger,
}

impl ScaleStep {
    fn steps(self) -> i8 {
        match self {
            Self::Smaller => -1,
            Self::Larger => 1,
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Smaller => "\u{2212}",
            Self::Larger => "+",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Smaller => "Smaller board text",
            Self::Larger => "Larger board text",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Smaller => "smaller",
            Self::Larger => "larger",
        }
    }
}

fn scale_button(
    state: &std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    workspace_id: WorkspaceId,
    colors: &BeadsBoardColors,
    step: ScaleStep,
) -> AnyElement {
    let state = std::sync::Arc::clone(state);
    div()
        .id(SharedString::from(format!("beads-text-{}-{workspace_id}", step.key())))
        .role(Role::Button)
        .aria_label(step.label())
        .w(px(ZOOM_GLYPH_W))
        .h(px(ZOOM_GLYPH_H))
        .flex()
        .items_center()
        .justify_center()
        .font_family("sans-serif")
        .text_size(px(11.0))
        .line_height(px(ZOOM_GLYPH_H))
        // Quiet at rest, lifting to title ink on hover/focus (A2-C8).
        .text_color(colors.quiet)
        .cursor_pointer()
        .hover(|button| button.text_color(colors.title))
        .on_hover({
            let state = std::sync::Arc::clone(&state);
            move |hovered: &bool, _window, _app| {
                if let Ok(mut boards) = state.lock() {
                    boards.hover(workspace_id, HoverSource::Control, *hovered);
                }
            }
        })
        // The grid below owns the pointer for selection, so the press stops
        // here rather than starting a drag in the terminal behind the board.
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, _app| {
            if let Ok(mut boards) = state.lock() {
                boards.adjust_text_scale(step.steps());
            }
            window.refresh();
        })
        .child(step.glyph())
        .into_any_element()
}

/// Everything a lane, tab, or row needs beyond its own [`QueueLane`]: where
/// to paint (the board's shared stores and colours), the two facts
/// [`beads_board_a2::layout`] resolves once per strip -- the scaled row
/// height, how many whole rows a lane's body reserves, and "now", for every
/// row's relative age -- and Blocked's/Done's own stable Tab stops, shared
/// by their tab, drawer, and pinned-lane unpin control alike.
#[derive(Clone, Copy)]
struct LaneCtx<'a> {
    stores: BoardStores<'a>,
    colors: &'a BeadsBoardColors,
    metrics: Metrics,
    row_height: f32,
    visible_rows: usize,
    now_epoch_s: i64,
    blocked_tab_focus: &'a FocusHandle,
    done_tab_focus: &'a FocusHandle,
}

impl<'a> LaneCtx<'a> {
    /// The one stable Tab stop `queue`'s collapsible-lane control keeps
    /// across every paint state -- tab, hot tab, and pinned-lane unpin
    /// (A2-I2's "activating the pinned tab or its `×` control unpins it").
    /// Every caller reaches this through [`collapsible_queue`] or
    /// [`queue_column`]'s own branch on a collapsed state, both of which
    /// admit only Blocked or Done -- an equality check rather than an
    /// exhaustive match so a third queue, though it never reaches here in
    /// practice, still returns a real handle instead of panicking (the
    /// workspace denies `clippy::unreachable`).
    fn tab_focus(&self, queue: BeadsIssueQueue) -> &'a FocusHandle {
        if queue == BeadsIssueQueue::Done { self.done_tab_focus } else { self.blocked_tab_focus }
    }
}

/// [`lanes`]'s own bundle of rail state plus the two stable Tab stops it
/// carries into [`LaneCtx`]: `RailState` alone cannot hold a GPUI
/// `FocusHandle` -- `beads_board_a2` stays paint-free by design -- so this is
/// where the two meet, and folding them into one parameter is what keeps
/// `lanes` itself under the workspace's five-argument ceiling.
#[derive(Clone, Copy)]
struct RailFocus<'a> {
    rail: RailState,
    blocked_tab_focus: &'a FocusHandle,
    done_tab_focus: &'a FocusHandle,
}

/// Paint A2: a unified hairline header (painted by the caller, see
/// [`headband`]) over five adaptive queue tracks, ending in the shared
/// floor. Reads every number from [`beads_board_a2::layout`] rather than
/// recomputing geometry here.
fn lanes(
    snapshot: &BeadsBoardSnapshot,
    stores: BoardStores<'_>,
    colors: &BeadsBoardColors,
    metrics: Metrics,
    rail_focus: RailFocus<'_>,
) -> AnyElement {
    let RailFocus { rail, blocked_tab_focus, done_tab_focus } = rail_focus;
    let now_epoch_s = i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs()),
    )
    .unwrap_or(i64::MAX);
    let layout = beads_board_a2::layout(A2Input {
        snapshot,
        rail,
        board_width: stores.rect.width,
        board_height: stores.rect.height,
        text_scale: metrics.scale,
    });
    let ctx = LaneCtx {
        stores,
        colors,
        metrics,
        row_height: layout.row_height,
        visible_rows: layout.visible_rows,
        now_epoch_s,
        blocked_tab_focus,
        done_tab_focus,
    };
    // A2-I1 opens at most one drawer per workspace, so at most one of these
    // is ever `Some`; the drawer paints after every track (see `lane_drawer`)
    // so it lays over the lanes as an overlay instead of joining their row.
    let open_drawer = if rail.blocked == CollapsedLaneState::Open {
        Some(lane_drawer(&layout.blocked, 3, colors.blocked_state, ctx))
    } else if rail.done == CollapsedLaneState::Open {
        Some(lane_drawer(&layout.done, 4, colors.done_state, ctx))
    } else {
        None
    };
    div()
        .relative()
        .h_full()
        .flex()
        .pt(px(beads_board_a2::LANES_PADDING_TOP))
        .pr(px(beads_board_a2::LANES_PADDING_RIGHT))
        .pb(px(beads_board_a2::LANES_PADDING_BOTTOM))
        .pl(px(beads_board_a2::LANES_PADDING_LEFT))
        .gap(px(beads_board_a2::TRACK_GAP))
        .child(queue_column(&layout.backlog, None, 0, colors.backlog_state, ctx))
        .child(queue_column(&layout.ready, None, 1, colors.ready_state, ctx))
        .child(queue_column(&layout.in_progress, None, 2, colors.progress_state, ctx))
        .child(queue_column(&layout.blocked, Some(rail.blocked), 3, colors.blocked_state, ctx))
        .child(queue_column(&layout.done, Some(rail.done), 4, colors.done_state, ctx))
        .children(open_drawer)
        .into_any_element()
}

/// One rail track: a full ledger lane for Backlog/Ready/In progress and for
/// a pinned Blocked/Done, or a plain collapsed tab for an unpinned
/// Blocked/Done (A2-S1, A2-S2). `collapsed` is `None` for the three queues
/// that are never collapsible.
fn queue_column(
    lane: &QueueLane<'_>,
    collapsed: Option<CollapsedLaneState>,
    lane_index: u8,
    state_color: Rgba,
    ctx: LaneCtx<'_>,
) -> AnyElement {
    // A drag in flight marks its hovered lane's left edge: the queue's own
    // hue for a lane that accepts the drop, muted for one that never writes
    // (A2-BD6). The pointer-to-lane geometry behind `drag_target` is still
    // the old five-equal-lanes math scribe-zwtv.9 owns updating for A2's
    // unequal tracks; this only re-paints the edge the index already named.
    let target_edge = (ctx.stores.drag_target == Some(lane_index))
        .then(|| if accepts_drop(lane_index) { state_color } else { ctx.colors.muted });
    match collapsed {
        Some(CollapsedLaneState::Tab) => collapsed_tab(lane, state_color, ctx, false, target_edge),
        Some(CollapsedLaneState::Open) => collapsed_tab(lane, state_color, ctx, true, target_edge),
        Some(CollapsedLaneState::Pinned) | None => {
            ledger_lane(lane, lane_index, state_color, ctx, target_edge)
        }
    }
}

/// Whether `lane_index`'s queue ever accepts a card drop (A2-BD6): Backlog
/// and Blocked never write, so their drag-target edge stays muted the way a
/// rejected target does.
fn accepts_drop(lane_index: u8) -> bool {
    !matches!(lane_index, 0 | 3)
}

/// A full A2 lane: head, seam, and either whole rows or void copy, with the
/// board's own overflow cue when the queue holds more than it shows
/// (A2-S1, A2-S3, A2-S5, A2-S6). [`queue_column`] only ever reaches this for
/// Blocked/Done when that queue is pinned, so [`lane_head`] can infer the
/// unpin control from `lane.queue` alone rather than a parameter here.
fn ledger_lane(
    lane: &QueueLane<'_>,
    lane_index: u8,
    state_color: Rgba,
    ctx: LaneCtx<'_>,
    target_edge: Option<Rgba>,
) -> AnyElement {
    div()
        .flex_none()
        .w(px(lane.width))
        .min_w(px(0.0))
        .relative()
        .flex()
        .flex_col()
        .child(lane_head(lane, state_color, ctx))
        .child(lane_seam(state_color, lane.void.is_some(), 0.0))
        .child(lane_body(lane, lane_index, state_color, ctx))
        .when(lane.overflow, |el| el.child(overflow_chevron(ctx.colors)))
        .when_some(target_edge, |el, edge| el.child(drag_target_edge(edge)))
        .into_any_element()
}

/// The left-edge mark a lane or tab wears while it is the drag in flight's
/// hovered target.
fn drag_target_edge(color: Rgba) -> AnyElement {
    div().absolute().top_0().bottom_0().left_0().w(px(1.0)).bg(color).into_any_element()
}

/// The lane head: a queue-tinted uppercase label beside its muted count, a
/// common epic hoisted to the far right when every visible row shares one
/// (A2-G6, A2-C2), and -- only for a pinned Blocked/Done lane -- the `×`
/// unpin control that replaces the tab this lane came from (A2-S3). Whether
/// that control appears is read straight off `lane.queue`: `ledger_lane`
/// only ever renders Blocked/Done here while pinned, so a collapsible queue
/// reaching this function is that invariant, not a flag threaded down for it.
fn lane_head(lane: &QueueLane<'_>, state_color: Rgba, ctx: LaneCtx<'_>) -> AnyElement {
    let colors = ctx.colors;
    let unpin_focus = collapsible_queue(lane.queue).then(|| ctx.tab_focus(lane.queue));
    let void = lane.void.is_some();
    // Header labels mix 40% queue hue toward chrome ink; empty labels use the
    // 32% muted treatment instead (A2-C2).
    let name_color = if void {
        mix(state_color, colors.muted, 0.68)
    } else {
        mix(state_color, colors.title, 0.6)
    };
    let count_color = if void { colors.muted } else { colors.queue_name };
    div()
        .flex_none()
        .h(px(beads_board_a2::HEAD_H))
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_size(px(9.5))
                .line_height(px(beads_board_a2::HEAD_H))
                .font_weight(FontWeight(700.0))
                .text_color(name_color)
                .child(queue_name(lane.queue).to_uppercase()),
        )
        .child(
            div()
                .flex_none()
                .font_family("monospace")
                .text_size(px(11.0))
                .line_height(px(beads_board_a2::HEAD_H))
                .font_weight(FontWeight(600.0))
                .text_color(count_color)
                .child(lane.total.to_string()),
        )
        .child(
            div()
                .ml_auto()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(9.0))
                .children(lane.epic.as_deref().map(|name| {
                    div()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(9.5))
                        .line_height(px(14.0))
                        .text_color(colors.muted)
                        .child(name.to_owned())
                }))
                .when_some(unpin_focus, |el, focus| el.child(unpin_control(lane, ctx, focus))),
        )
        .into_any_element()
}

/// A lane's 2px state seam: full queue hue fading to 12% of itself, or 34%
/// fading to 9% for an empty lane (A2-C5). `bleed` extends the seam past its
/// own horizontal edges by that many pixels each side with a negative
/// margin, which is how the drawer's seam reaches the drawer's own edges
/// past its 13px padding (A2-G8) instead of stopping at the padded content
/// width a plain lane's seam already fills exactly.
fn lane_seam(state_color: Rgba, void: bool, bleed: f32) -> AnyElement {
    let (from, to) = if void {
        (alpha(state_color, 0.34), alpha(state_color, 0.09))
    } else {
        (state_color, alpha(state_color, 0.12))
    };
    div()
        .flex_none()
        .mx(px(-bleed))
        .h(px(beads_board_a2::SEAM_H))
        .bg(linear_gradient(90.0, linear_color_stop(from, 0.0), linear_color_stop(to, 1.0)))
        .into_any_element()
}

/// The pinned lane head's `×`: activating it unpins the same way
/// reactivating the collapsed tab would (A2-I2). Shares `unpin_focus` --
/// [`BeadsBoardRender::blocked_tab_focus`]/`done_tab_focus` -- with the tab
/// this lane replaces, so Tab order does not gain or lose a stop when a
/// lane pins or unpins.
fn unpin_control(lane: &QueueLane<'_>, ctx: LaneCtx<'_>, focus: &FocusHandle) -> AnyElement {
    let queue = lane.queue;
    let workspace_id = ctx.stores.workspace_id;
    let click_boards = std::sync::Arc::clone(ctx.stores.boards);
    let key_boards = std::sync::Arc::clone(ctx.stores.boards);
    let click_focus = focus.clone();
    let quiet = ctx.colors.quiet;
    let title = ctx.colors.title;
    div()
        .id(SharedString::from(format!("beads-unpin-{workspace_id}-{queue:?}")))
        .role(Role::Button)
        .aria_label(unpin_accessible_label(lane))
        .track_focus(focus)
        .tab_stop(true)
        .focus_visible(move |style| style.border_1().border_color(title))
        .flex_none()
        .cursor_pointer()
        .text_size(px(13.0))
        .line_height(px(beads_board_a2::HEAD_H))
        .text_color(quiet)
        .hover(move |el| el.text_color(title))
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, app| {
            window.focus(&click_focus, app);
            if let Ok(mut boards) = click_boards.lock() {
                boards.unpin_lane(workspace_id, queue);
            }
            window.refresh();
        })
        .on_key_down(move |event: &KeyDownEvent, window, app| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                app.stop_propagation();
                if let Ok(mut boards) = key_boards.lock() {
                    boards.unpin_lane(workspace_id, queue);
                }
                window.refresh();
            }
        })
        .child("×")
        .into_any_element()
}

/// [`unpin_control`]'s accessible name (A2-I3): queue, count, pinned state,
/// and the unpin action -- the pinned twin of [`tab_accessible_label`].
fn unpin_accessible_label(lane: &QueueLane<'_>) -> String {
    format!("Unpin {} lane, {} issues, pinned", queue_name(lane.queue), lane.total)
}

/// The lane's fixed-height row box (A2-G4, A2-G10): always tall enough for
/// [`LaneCtx::visible_rows`] whole rows, whether this queue's own rows fill
/// it or a shorter list leaves ground beneath, so every lane's floor lines
/// up regardless of how many rows it actually holds.
fn lane_body(
    lane: &QueueLane<'_>,
    lane_index: u8,
    state_color: Rgba,
    ctx: LaneCtx<'_>,
) -> AnyElement {
    let height = ctx.row_height * count_to_f32(ctx.visible_rows);
    let body = div().flex_none().h(px(height)).overflow_hidden().flex().flex_col();
    if let Some(void) = &lane.void {
        return body.child(void_copy(void, ctx.colors, ctx.metrics)).into_any_element();
    }
    let last = lane.rows.len().saturating_sub(1);
    body.children(lane.rows.iter().enumerate().map(|(index, row)| {
        let card = CardContext {
            workspace_id: ctx.stores.workspace_id,
            state: ctx.stores.boards,
            panels: ctx.stores.panels,
            lane: lane_index,
            board_rect: ctx.stores.rect,
            colors: ctx.colors,
            metrics: ctx.metrics,
        };
        ledger_row(
            row,
            RowMeta {
                state_color,
                show_separator: index < last,
                row_height: ctx.row_height,
                now_epoch_s: ctx.now_epoch_s,
            },
            card,
        )
    }))
    .into_any_element()
}

/// Queue-specific empty-lane copy, aligned under where a row's title would
/// start; Ready's subordinate blocked-count hint prints as its own line
/// beneath the headline (A2-S5).
fn void_copy(void: &VoidCopy, colors: &BeadsBoardColors, metrics: Metrics) -> AnyElement {
    div()
        .pl(metrics.at(ROW_TITLE_INDENT))
        .pt(metrics.at(3.0))
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(16.0))
                .text_color(colors.quiet)
                .child(void.headline),
        )
        .children(void.subordinate.as_deref().map(|subordinate| {
            div()
                .font_family("monospace")
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(15.0))
                .font_weight(FontWeight(500.0))
                .text_color(colors.quiet)
                .child(subordinate.to_owned())
        }))
        .into_any_element()
}

/// The board's own `⌄` mark: a lane holds more rows than its fixed body
/// shows (A2-G9, A2-S6).
fn overflow_chevron(colors: &BeadsBoardColors) -> AnyElement {
    div()
        .absolute()
        .right(px(CHEV_RIGHT))
        .bottom(px(CHEV_BOTTOM))
        .text_size(px(CHEV_SIZE))
        .line_height(px(CHEV_SIZE))
        .text_color(colors.quiet)
        .child("⌄")
        .into_any_element()
}

/// A collapsed Blocked/Done rail tab (A2-S1, A2-S2): count, fading state
/// seam, and a one-glyph-per-line spine -- GPUI has no text-rotation
/// primitive, so a stacked spine is the only vertical label it can paint --
/// plus the cue that says a drawer lives here and the hot lift/inner-edge
/// treatment while its drawer is open. Pointer/keyboard wiring (hover or
/// focus opens the drawer, click or Enter/Space pins it) is
/// [`tab_interactivity`]'s.
fn collapsed_tab(
    lane: &QueueLane<'_>,
    state_color: Rgba,
    ctx: LaneCtx<'_>,
    hot: bool,
    target_edge: Option<Rgba>,
) -> AnyElement {
    let colors = ctx.colors;
    let void = lane.void.is_some();
    let title = colors.title;
    let spine_color = if hot {
        title
    } else if void {
        colors.quiet
    } else {
        mix(state_color, title, 0.6)
    };
    let count_color = if hot {
        title
    } else if void {
        colors.quiet
    } else {
        colors.queue_name
    };
    let cue_color = if hot { title } else { colors.quiet };
    // A hot cue's chip reads roughly twice as strong as the tab's own hover
    // lift, the same ratio the mock's `#ffffff1a` chip over a `#ffffff0d` tab
    // carries (A2-C6).
    let cue_hot_bg = alpha(title, 0.16);
    let (seam_from, seam_to) = if void {
        (alpha(state_color, 0.34), alpha(state_color, 0.09))
    } else {
        (state_color, alpha(state_color, 0.12))
    };
    let spine = queue_name(lane.queue).to_uppercase();

    tab_interactivity(
        div().relative().flex_none().w(px(lane.width)).flex().flex_col().items_center(),
        lane,
        ctx,
        hot,
    )
    .child(
        div()
            .flex_none()
            .h(px(beads_board_a2::HEAD_H))
            .flex()
            .items_center()
            .font_family("monospace")
            .text_size(px(11.0))
            .text_color(count_color)
            .child(lane.total.to_string()),
    )
    .child(div().flex_none().w_full().h(px(beads_board_a2::SEAM_H)).bg(linear_gradient(
        90.0,
        linear_color_stop(seam_from, 0.0),
        linear_color_stop(seam_to, 1.0),
    )))
    .child(tab_spine(&spine, spine_color))
    .child(tab_cue(cue_color, hot, cue_hot_bg))
    .when(hot, |el| el.child(hot_inner_edge(state_color)))
    .when_some(target_edge, |el, edge| el.child(drag_target_edge(edge)))
    .into_any_element()
}

/// [`collapsed_tab`]'s pointer/keyboard wiring, split out to keep that
/// function under the workspace's line ceiling: id, AccessKit role/name,
/// Tab stop and its visible-focus ring, the hover/hot background lift, and
/// hover-opens/click-or-Enter/Space-pins behaviour, all against the one
/// stable `ctx.tab_focus` handle this queue's collapsible-lane control keeps
/// across every paint state (tab, hot tab, and pinned via
/// [`unpin_control`]), so Tab order never gains or loses a stop when it pins
/// or unpins. The paired mouse-down/mouse-up stops are the rule scribe-uu2y
/// established for every new click target.
fn tab_interactivity(
    base: gpui::Div,
    lane: &QueueLane<'_>,
    ctx: LaneCtx<'_>,
    hot: bool,
) -> gpui::Stateful<gpui::Div> {
    let queue = lane.queue;
    let workspace_id = ctx.stores.workspace_id;
    let focus = ctx.tab_focus(queue);
    let hover_boards = std::sync::Arc::clone(ctx.stores.boards);
    let click_boards = std::sync::Arc::clone(ctx.stores.boards);
    let key_boards = std::sync::Arc::clone(ctx.stores.boards);
    let click_focus = focus.clone();
    let title = ctx.colors.title;
    let button_hover = ctx.colors.button_hover;
    base.id(SharedString::from(format!("beads-tab-{workspace_id}-{queue:?}")))
        .role(Role::Button)
        .aria_label(tab_accessible_label(lane))
        .track_focus(focus)
        .tab_stop(true)
        .focus_visible(move |style| style.border_1().border_color(title))
        .cursor_pointer()
        .when(hot, |el| el.bg(button_hover))
        .hover(move |el| el.bg(button_hover))
        .on_hover(move |entered: &bool, _window, _app| {
            if let Ok(mut boards) = hover_boards.lock() {
                boards.hover_lane(workspace_id, queue, LaneHoverSource::Tab, *entered);
            }
        })
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, app| {
            window.focus(&click_focus, app);
            if let Ok(mut boards) = click_boards.lock() {
                boards.pin_lane(workspace_id, queue);
            }
            window.refresh();
        })
        .on_key_down(move |event: &KeyDownEvent, window, app| {
            if !event.keystroke.modifiers.modified()
                && matches!(event.keystroke.key.as_str(), "enter" | "space")
            {
                app.stop_propagation();
                if let Ok(mut boards) = key_boards.lock() {
                    boards.pin_lane(workspace_id, queue);
                }
                window.refresh();
            }
        })
}

/// The tab's vertically centred letterform spine, one glyph per line box
/// (A2-G7): GPUI has no text-rotation primitive, so this is the only
/// vertical label it can paint. Each glyph `div` carries neither an id nor a
/// role, so none is ever reported to AccessKit on its own -- the spine reads
/// as the tab's one accessible name, never as separate letters.
fn tab_spine(spine: &str, color: Rgba) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(1.0))
        .children(spine.chars().map(move |glyph| {
            div()
                .text_size(px(9.5))
                .line_height(px(10.5))
                .font_weight(FontWeight(700.0))
                .text_color(color)
                .child(glyph.to_string())
        }))
        .into_any_element()
}

/// The tab's own `‹` cue: always present, brightening to a lifted chip while
/// the tab is hot (A2-G7, A2-C6).
fn tab_cue(color: Rgba, hot: bool, hot_bg: Rgba) -> AnyElement {
    div()
        .flex_none()
        .mb(px(TAB_CUE_MARGIN_BOTTOM))
        .w(px(TAB_CUE_SIZE))
        .h(px(TAB_CUE_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(TAB_CUE_RADIUS))
        .when(hot, |el| el.bg(hot_bg))
        .text_size(px(11.0))
        .line_height(px(TAB_CUE_SIZE))
        .text_color(color)
        .child("\u{2039}")
        .into_any_element()
}

/// A hot tab's 1px queue-hue inner edge (A2-C6): from just under the seam to
/// the tab's own bottom, so an open tab and the drawer it opened read as one
/// object rather than two.
fn hot_inner_edge(color: Rgba) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .top(px(beads_board_a2::SEAM_H))
        .bottom_0()
        .w(px(1.0))
        .bg(color)
        .into_any_element()
}

/// [`collapsed_tab`]'s accessible name (A2-I3): queue, count, collapsed
/// state, and what focus/activation do -- read as one queue label, never per
/// spine glyph, since the individual spine letters carry neither an id nor a
/// role and so are never reported to AccessKit on their own.
fn tab_accessible_label(lane: &QueueLane<'_>) -> String {
    format!(
        "{} lane, {} issues, collapsed. Focus previews, activate pins",
        queue_name(lane.queue),
        lane.total
    )
}

/// The transient, non-reflowing drawer a collapsed tab opens on hover or
/// keyboard focus (A2-S2, A2-G8): the same row geometry a pinned lane uses,
/// laid over the lanes as an absolute overlay -- painted after every track in
/// [`lanes`] so it lays over them -- rather than joining their flex row, so
/// opening it never reflows them (A2-I1). `occlude` keeps a click on the
/// drawer's own chrome from also landing on whatever lane row it visually
/// covers; the paired mouse-down/mouse-up stops keep that same click from
/// reaching the terminal underneath, the rule scribe-uu2y established for
/// every new click target.
fn lane_drawer(
    lane: &QueueLane<'_>,
    lane_index: u8,
    state_color: Rgba,
    ctx: LaneCtx<'_>,
) -> AnyElement {
    let queue = lane.queue;
    let workspace_id = ctx.stores.workspace_id;
    let hover_boards = std::sync::Arc::clone(ctx.stores.boards);
    let click_boards = std::sync::Arc::clone(ctx.stores.boards);
    let click_focus = ctx.tab_focus(queue).clone();
    let border = alpha(ctx.colors.title, 0.15);
    div()
        .id(SharedString::from(format!("beads-drawer-{workspace_id}-{queue:?}")))
        .role(Role::Group)
        .aria_label(format!("{} preview, {} issues", queue_name(queue), lane.total))
        .absolute()
        .top(px(beads_board_a2::DRAWER_TOP))
        .bottom(px(beads_board_a2::DRAWER_BOTTOM))
        .right(px(beads_board_a2::DRAWER_RIGHT))
        .w(px(beads_board_a2::DRAWER_W))
        .relative()
        .occlude()
        .flex()
        .flex_col()
        .px(px(beads_board_a2::DRAWER_PAD_H))
        .bg(ctx.colors.band)
        .border_1()
        .border_color(border)
        .rounded(px(beads_board_a2::DRAWER_RADIUS))
        .shadow_lg()
        .on_hover(move |entered: &bool, _window, _app| {
            if let Ok(mut boards) = hover_boards.lock() {
                boards.hover_lane(workspace_id, queue, LaneHoverSource::Drawer, *entered);
            }
        })
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_mouse_up(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, window, app| {
            window.focus(&click_focus, app);
            if let Ok(mut boards) = click_boards.lock() {
                boards.pin_lane(workspace_id, queue);
            }
            window.refresh();
        })
        .child(drawer_head(lane, state_color, ctx.colors))
        .child(lane_seam(state_color, lane.void.is_some(), beads_board_a2::DRAWER_PAD_H))
        .child(lane_body(lane, lane_index, state_color, ctx))
        .when(lane.overflow, |el| el.child(overflow_chevron(ctx.colors)))
        .into_any_element()
}

/// The drawer's own head: name and count always at full strength (unlike a
/// void lane head, the drawer never dims them), an optional shared epic, and
/// the `click to pin` hint the mock's `pinhint` carries.
fn drawer_head(lane: &QueueLane<'_>, state_color: Rgba, colors: &BeadsBoardColors) -> AnyElement {
    let name_color = mix(state_color, colors.title, 0.6);
    div()
        .flex_none()
        .h(px(beads_board_a2::HEAD_H))
        .flex()
        .items_baseline()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_size(px(9.5))
                .line_height(px(beads_board_a2::HEAD_H))
                .font_weight(FontWeight(700.0))
                .text_color(name_color)
                .child(queue_name(lane.queue).to_uppercase()),
        )
        .child(
            div()
                .flex_none()
                .font_family("monospace")
                .text_size(px(11.0))
                .line_height(px(beads_board_a2::HEAD_H))
                .font_weight(FontWeight(600.0))
                .text_color(colors.queue_name)
                .child(lane.total.to_string()),
        )
        .children(lane.epic.as_deref().map(|name| {
            div()
                .ml(px(2.0))
                .min_w(px(0.0))
                .truncate()
                .text_size(px(9.5))
                .line_height(px(14.0))
                .text_color(colors.muted)
                .child(name.to_owned())
        }))
        .child(
            div()
                .ml_auto()
                .flex_none()
                .font_family("monospace")
                .text_size(px(9.5))
                .line_height(px(beads_board_a2::HEAD_H))
                .font_weight(FontWeight(500.0))
                .text_color(colors.quiet)
                .child("click to pin"),
        )
        .into_any_element()
}

/// One visible row's own paint facts, separate from [`CardContext`] because
/// they come from the lane's [`QueueLane`]/position rather than the board's
/// shared stores.
#[derive(Clone, Copy)]
struct RowMeta {
    state_color: Rgba,
    /// Whether this row owns the hairline under it: every row but a lane's
    /// last visible one. Hover replaces this same edge with a 2px lane-hue
    /// underline regardless, so only the separator itself is conditional
    /// (A2-S7).
    show_separator: bool,
    row_height: f32,
    now_epoch_s: i64,
}

/// One 51px ledger row: a saturated priority glyph, the title, and a
/// three-column sub line, opening the issue exactly as the raised card it
/// replaces did (A2-G4, A2-G5, A2-C3).
fn ledger_row(row: &RowView<'_>, meta: RowMeta, card: CardContext<'_>) -> AnyElement {
    let RowMeta { state_color, show_separator, row_height, now_epoch_s } = meta;
    let CardContext { workspace_id, state, panels, lane, board_rect, colors, metrics } = card;
    let item = row.item;
    let panels = std::sync::Arc::clone(panels);
    let selected = item.clone();
    let dragged = item.clone();
    let arm_state = std::sync::Arc::clone(state);
    let click_state = std::sync::Arc::clone(state);
    let start_state = std::sync::Arc::clone(state);
    let draggable = card_drag_source(lane);
    let drag_source = item.clone();
    let drag_colors = *colors;
    let mark = colors.priority(item.priority);
    let tooltip_bounds = std::rc::Rc::new(std::cell::Cell::new(None));
    let measured_tooltip_bounds = std::rc::Rc::clone(&tooltip_bounds);
    let row_element = div()
        .on_children_prepainted(move |children, _window, _app| {
            measured_tooltip_bounds.set(children.first().copied());
        })
        .id(SharedString::from(format!("beads-row-{workspace_id}-{}", item.id)))
        .role(Role::Button)
        .aria_label(format!("Open issue {}", item.id))
        .relative()
        .flex_none()
        .h(px(row_height))
        .flex()
        .flex_col()
        .justify_center()
        .gap(metrics.at(beads_board_a2::ROW_INTERLINE_GAP))
        .cursor_pointer()
        .when(show_separator, |el| el.border_b_2().border_color(colors.hairline))
        .hover(|el| el.bg(colors.button_hover).border_b_2().border_color(state_color))
        .on_mouse_down(MouseButton::Left, move |event, _window, app| {
            if let Ok(mut boards) = arm_state.lock() {
                let pointer = card_drag_point(event.position);
                boards.arm_card_drag(workspace_id, dragged.clone(), lane, pointer);
            }
            app.stop_propagation();
        })
        .on_click(move |_event, window, _app| {
            // The panel opens exactly as it always has; the strip only follows
            // when this card names an epic the server will serve a graph for.
            if let Ok(mut panels) = panels.lock() {
                panels.open(workspace_id, selected.clone(), lane);
            }
            if let Ok(mut boards) = click_state.lock() {
                boards.request_card_flow(workspace_id, &selected);
            }
            window.refresh();
        });
    let row_element = if draggable {
        row_element.on_drag(BeadsCardDrag, move |_, _, window, app| {
            let ghost = start_state
                .lock()
                .map_or(None, |mut boards| {
                    boards.start_card_drag(card_drag_point(window.mouse_position()), board_rect);
                    boards.card_drag_ghost(board_rect, metrics.scale)
                })
                .unwrap_or_else(|| {
                    CardDragGhost::new(drag_source.clone(), board_rect, metrics.scale)
                });
            window.refresh();
            app.new(move |_| BeadsCardDragGhost { model: ghost, colors: drag_colors, metrics })
        })
    } else {
        row_element
    };
    with_card_title_tooltip(
        row_element,
        item.title.clone(),
        tooltip_bounds,
        colors,
        metrics.at(12.0),
    )
    .child(row_title_line(item, mark, colors, metrics))
    .child(row_sub_line(row, now_epoch_s, card))
    .into_any_element()
}

/// The row's top line: a 20px saturated priority glyph -- the row's only
/// saturated ink -- then the title (A2-G5, A2-C3).
fn row_title_line(
    item: &BeadsBoardItem,
    mark: Rgba,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> AnyElement {
    div()
        .flex_none()
        .h(metrics.at(beads_board_a2::ROW_TITLE_H))
        .flex()
        .gap(metrics.at(beads_board_a2::ROW_PRIORITY_GAP))
        .child(
            div()
                .flex_none()
                .w(metrics.at(beads_board_a2::ROW_PRIORITY_W))
                .font_family("monospace")
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(beads_board_a2::ROW_TITLE_H))
                .font_weight(FontWeight(700.0))
                .text_color(mark)
                .child(format!("P{}", item.priority)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(metrics.at(12.0))
                .line_height(metrics.at(beads_board_a2::ROW_TITLE_H))
                .font_weight(FontWeight(600.0))
                .text_color(colors.title)
                .child(item.title.clone()),
        )
        .into_any_element()
}

/// The row's sub line: ID left (copyable), age at the row's true centre, and
/// an optional epic right with at least [`beads_board_a2::EPIC_SEPARATION_MIN`]
/// clearance -- three independent slots, so the age never moves with
/// whether a row carries an epic (A2-G5).
fn row_sub_line(row: &RowView<'_>, now_epoch_s: i64, card: CardContext<'_>) -> AnyElement {
    let CardContext { workspace_id, state, colors, metrics, .. } = card;
    let item = row.item;
    div()
        .flex_none()
        .h(metrics.at(beads_board_a2::ROW_SUB_H))
        .pl(metrics.at(ROW_TITLE_INDENT))
        .flex()
        .items_center()
        .child(
            div().flex_1().min_w(px(0.0)).child(copyable(
                CopyTarget {
                    key: format!("beads-id-{workspace_id}-{}", item.id),
                    label: format!("Copy issue {}", item.id),
                    text: item.id.clone(),
                    shown: short_id(&item.id).to_owned(),
                    state,
                },
                div()
                    .flex_none()
                    .font_family("monospace")
                    .text_size(metrics.at(9.5))
                    .line_height(metrics.at(beads_board_a2::ROW_SUB_H))
                    .font_weight(FontWeight(500.0))
                    .text_color(colors.muted),
                colors,
            )),
        )
        .child(
            div()
                .flex_none()
                .font_family("monospace")
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(beads_board_a2::ROW_SUB_H))
                .font_weight(FontWeight(500.0))
                .text_color(colors.quiet)
                .children(compact_relative_age(&item.updated_at, now_epoch_s)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .pl(metrics.at(beads_board_a2::EPIC_SEPARATION_MIN))
                .flex()
                .justify_end()
                .children(row.epic.as_deref().zip(item.parent_epic_name.as_deref()).map(
                    |(shown, full)| {
                        copyable(
                            CopyTarget {
                                key: format!("beads-epic-{workspace_id}-{}", item.id),
                                label: format!("Copy epic {full}"),
                                text: full.to_owned(),
                                shown: shown.to_owned(),
                                state,
                            },
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(metrics.at(9.5))
                                .line_height(metrics.at(beads_board_a2::ROW_SUB_H))
                                .text_color(colors.muted),
                            colors,
                        )
                    },
                )),
        )
        .into_any_element()
}

fn with_card_title_tooltip(
    card: gpui::Stateful<gpui::Div>,
    title: String,
    popup_bounds: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    colors: &BeadsBoardColors,
    text_size: Pixels,
) -> gpui::Stateful<gpui::Div> {
    let colors = *colors;
    card.tooltip(move |_window, cx| {
        let anchor = popup_bounds.get().unwrap_or_default();
        cx.new(|_| BeadsCardTooltip { title: title.clone(), anchor, colors, text_size }).into()
    })
    .tooltip_show_delay(Duration::ZERO)
}

/// The priority badge and the title, which owns the rest of its line: the
/// title is the only line a reader scans, so nothing else shares its row.
fn issue_title(
    item: &BeadsBoardItem,
    mark: PriorityMark,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> gpui::Div {
    div()
        .h(metrics.at(TITLE_LINE))
        .flex()
        .items_center()
        .gap(px(5.0))
        .child(
            div()
                .flex_none()
                .rounded(px(3.0))
                .px(px(4.0))
                .bg(mark.fill)
                .font_family("monospace")
                .text_size(metrics.at(9.5))
                .line_height(metrics.at(13.0))
                .font_weight(FontWeight(700.0))
                .text_color(mark.ink)
                .child(format!("P{}", item.priority)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(metrics.at(12.0))
                .line_height(metrics.at(TITLE_LINE))
                .font_weight(FontWeight(650.0))
                .text_color(colors.title)
                .child(item.title.clone()),
        )
}

/// The ghost's non-interactive metadata; its source card remains the only
/// clipboard and panel target during the gesture.
fn drag_ghost_meta(
    item: &BeadsBoardItem,
    colors: &BeadsBoardColors,
    metrics: Metrics,
) -> AnyElement {
    div()
        .h(metrics.at(12.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .font_family("monospace")
                .text_size(metrics.at(9.0))
                .line_height(metrics.at(12.0))
                .font_weight(FontWeight(500.0))
                .text_color(colors.muted)
                .child(short_id(&item.id).to_owned()),
        )
        .child(div().flex_1().min_w(px(0.0)))
        .children(item.parent_epic_name.as_ref().map(|name| {
            div()
                .min_w(px(0.0))
                .ml(px(8.0))
                .truncate()
                .text_right()
                .text_size(metrics.at(9.0))
                .line_height(metrics.at(12.0))
                .text_color(colors.epic)
                .child(short_epic(name))
        }))
        .into_any_element()
}

/// What every card on one board shares.
#[derive(Clone, Copy)]
struct CardContext<'a> {
    workspace_id: WorkspaceId,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
    panels: &'a std::sync::Arc<std::sync::Mutex<BeadsPanels>>,
    lane: u8,
    board_rect: Rect,
    colors: &'a BeadsBoardColors,
    metrics: Metrics,
}

/// One line of card metadata the pointer can lift onto the clipboard.
struct CopyTarget<'a> {
    key: String,
    label: String,
    /// What lands on the clipboard, which is the full id even where the card
    /// shows the short one: a shortened id is not one anything else accepts.
    text: String,
    shown: String,
    state: &'a std::sync::Arc<std::sync::Mutex<BeadsBoards>>,
}

/// Make `styled` a click-to-copy target for `target`.
fn copyable(target: CopyTarget<'_>, styled: gpui::Div, colors: &BeadsBoardColors) -> AnyElement {
    let CopyTarget { key, label, text, shown, state } = target;
    let state = std::sync::Arc::clone(state);
    let hover = colors.title;
    styled
        .id(SharedString::from(key))
        .role(Role::Button)
        .aria_label(label)
        .cursor_pointer()
        .hover(move |line| line.text_color(hover))
        // The grid below owns the pointer for selection, so the press stops
        // here rather than starting a drag in the terminal behind the board.
        .on_mouse_down(MouseButton::Left, |_, _window, app| app.stop_propagation())
        .on_click(move |_event, _window, _app| {
            if let Ok(mut boards) = state.lock() {
                boards.copy(text.clone());
            }
        })
        .child(shown)
        .into_any_element()
}

/// How much of an epic name a card shows before it is cut short, and the
/// shortest prefix a word boundary may leave. Measured against every epic in
/// this machine's Beads projects: a median name is 21 characters and one word,
/// so the cap leaves half of them untouched and trims the long tail.
const EPIC_MAX_CHARS: usize = 24;

/// An epic name shortened to what tells one epic from another.
///
/// Cut at a word boundary — space, hyphen, underscore, or slash, so a
/// slug-style name breaks as readably as a sentence one — and only when that
/// boundary leaves at least half the budget; otherwise the cut is hard, since
/// a boundary near the start would throw away more than it saves.
///
/// `pub(crate)` so the A2 presentation model
/// ([`crate::beads_board_a2`]) can reuse the same truncation for a lane's
/// hoisted and per-row epic text instead of re-deriving it.
pub(crate) fn short_epic(name: &str) -> String {
    let mut head: String = name.chars().take(EPIC_MAX_CHARS).collect();
    if head.chars().count() == name.chars().count() {
        return head;
    }
    if let Some(boundary) = head.rfind([' ', '-', '_', '/'])
        && head[..boundary].chars().count() >= EPIC_MAX_CHARS / 2
    {
        head.truncate(boundary);
    }
    while head.ends_with([' ', '-', '_', '/']) {
        head.pop();
    }
    head.push('\u{2026}');
    head
}

/// An issue id without the project it belongs to: `nasha-lab-byd.12` reads as
/// `byd.12`, and a board of one project repeats that prefix on every card.
///
/// The tail after the last `-`, so a project whose own name carries one keeps
/// working. An id that is all prefix, or has no `-` at all, is left alone —
/// half an id is worse than a long one.
pub(crate) fn short_id(id: &str) -> &str {
    id.rsplit_once('-').map_or(id, |(_, tail)| if tail.is_empty() { id } else { tail })
}

fn board_content(state: Option<&BeadsBoardState>) -> (Option<&BeadsBoardSnapshot>, String) {
    match state {
        Some(BeadsBoardState::Ready { snapshot, .. }) => (Some(snapshot), String::new()),
        Some(BeadsBoardState::Loading { cached: Some(snapshot) }) => {
            (Some(snapshot), String::new())
        }
        Some(BeadsBoardState::Loading { cached: None }) | None => (None, "Loading board…".into()),
        Some(BeadsBoardState::Unavailable { message }) => (None, message.clone()),
        Some(BeadsBoardState::NotDetected) => (None, "No Beads project".into()),
    }
}

/// `color` lifted away from `ground` until it clears `min_ratio`.
///
/// Flattened onto the ground first, so an alpha-reduced slot is measured as it
/// is seen, then mixed toward white or black — whichever direction the ground
/// is not — in sixteenths. A colour already clearing the floor comes back
/// untouched, so a theme with good contrast keeps its own tones exactly.
fn readable(color: Rgba, ground: Rgba, min_ratio: f32) -> Rgba {
    let flat = over(color, ground);
    if contrast(flat, ground) >= min_ratio {
        return flat;
    }
    let target = if luminance(ground) < 0.5 { WHITE } else { BLACK };
    let mut lifted = flat;
    for step in 1..=16_u8 {
        lifted = mix(flat, target, f32::from(step) / 16.0);
        if contrast(lifted, ground) >= min_ratio {
            break;
        }
    }
    lifted
}

/// `color` lifted until it clears `min_ratio` on both grounds it is read on.
///
/// The strip's ground and a card's raised fill pull opposite ways depending on
/// the theme's polarity, so neither is the strict one to measure against. Two
/// passes settle it: the second lifts further only if the first left the ink
/// short on the other ground.
fn readable_anywhere(color: Rgba, ground: Rgba, card: Rgba, min_ratio: f32) -> Rgba {
    readable(readable(color, ground, min_ratio), card, min_ratio)
}

/// `color` composited over `ground`, so an alpha carries into a solid colour.
fn over(color: Rgba, ground: Rgba) -> Rgba {
    mix(ground, Rgba { a: 1.0, ..color }, color.a)
}

/// `from` moved `amount` of the way toward `to`.
fn mix(from: Rgba, to: Rgba, amount: f32) -> Rgba {
    let amount = amount.clamp(0.0, 1.0);
    Rgba {
        r: (to.r - from.r).mul_add(amount, from.r),
        g: (to.g - from.g).mul_add(amount, from.g),
        b: (to.b - from.b).mul_add(amount, from.b),
        a: 1.0,
    }
}

/// WCAG contrast ratio between two opaque colours.
pub(crate) fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (high, low) = {
        let (a, b) = (luminance(a), luminance(b));
        if a >= b { (a, b) } else { (b, a) }
    };
    (high + 0.05) / (low + 0.05)
}

/// WCAG relative luminance, which linearises each channel first.
fn luminance(color: Rgba) -> f32 {
    fn linear(channel: f32) -> f32 {
        if channel <= 0.040_45 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }
    }
    0.2126f32.mul_add(linear(color.r), 0.7152f32.mul_add(linear(color.g), 0.0722 * linear(color.b)))
}

/// One theme slot as a colour, keeping the alpha the derivation gave it.
fn slot(color: [f32; 4]) -> Rgba {
    Rgba { r: color[0], g: color[1], b: color[2], a: color[3] }
}

fn alpha(color: Rgba, a: f32) -> Rgba {
    Rgba { a, ..color }
}
#[cfg(test)]
mod tests {
    use scribe_common::theme::ChromeColors;

    use super::*;

    /// Chrome slots the board never reads, filled so a test can name only the
    /// few it does.
    fn chrome_slots(fill: [f32; 4]) -> ChromeColors {
        ChromeColors {
            tab_bar_bg: fill,
            tab_bar_active_bg: fill,
            tab_text: fill,
            tab_text_active: fill,
            tab_separator: fill,
            status_bar_bg: fill,
            status_bar_text: fill,
            divider: fill,
            accent: fill,
            scrollbar: fill,
            tab_bar_gradient_top: fill,
            status_bar_separator: fill,
            prompt_bar_first_row_bg: fill,
            prompt_bar_second_row_bg: fill,
            prompt_bar_text: fill,
            prompt_bar_icon_first: fill,
            prompt_bar_icon_latest: fill,
        }
    }

    fn visible_sorted(boards: &BeadsBoards) -> Vec<(WorkspaceId, bool)> {
        let mut visible = boards.visible();
        visible.sort_by_key(|(workspace_id, _)| workspace_id.to_string());
        visible
    }

    fn drag_item() -> BeadsBoardItem {
        BeadsBoardItem {
            id: "scribe-drag.1".into(),
            title: "Track the card drag".into(),
            priority: 1,
            blocker_ids: Vec::new(),
            parent_epic_name: Some("Beads card detail".into()),
            parent_epic_id: Some("scribe-5wh1".into()),
            updated_at: String::new(),
        }
    }

    fn drag_point(x: f32, y: f32) -> CardDragPoint {
        CardDragPoint { x, y }
    }

    fn drag_board() -> Rect {
        Rect { x: 100.0, y: 50.0, width: 500.0, height: 200.0 }
    }

    fn drag_state(workspace_id: WorkspaceId, source_lane: u8, target_lane: u8) -> CardDragState {
        CardDragState {
            workspace_id,
            source: drag_item(),
            source_lane,
            pointer: drag_point(0.0, 0.0),
            hovered_lane: Some(target_lane),
        }
    }

    fn drag_snapshot(lane: u8) -> BeadsBoardState {
        let mut snapshot = BeadsBoardSnapshot::default();
        if let Some(cards) = lane_cards_mut(&mut snapshot, lane) {
            cards.push(drag_item());
        }
        if let Some(total) = lane_total_mut(&mut snapshot, lane) {
            *total = 1;
        }
        BeadsBoardState::Ready { snapshot, stale: false, refresh_error: None }
    }

    fn board_card_lane(boards: &BeadsBoards, workspace_id: WorkspaceId) -> Option<u8> {
        let snapshot = match boards.state(workspace_id)? {
            BeadsBoardState::Loading { cached } => cached.as_ref(),
            BeadsBoardState::Ready { snapshot, .. } => Some(snapshot),
            BeadsBoardState::NotDetected | BeadsBoardState::Unavailable { .. } => None,
        }?;
        snapshot_card_lane(snapshot, "scribe-drag.1")
    }

    // @lat: [[test#GPUI Client Headless Suites#Beads card drag tracking]]
    #[test]
    fn card_drag_arms_only_past_the_native_two_pixel_boundary() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();

        assert!(boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0)));
        assert!(!boards.start_card_drag(drag_point(152.0, 100.0), drag_board()));
        assert!(boards.card_drag().is_none(), "exactly 2px must remain a click");
        assert!(!boards.end_card_drag(), "a within-threshold release swallowed the click");

        assert!(boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0)));
        assert!(boards.start_card_drag(drag_point(152.001, 100.0), drag_board()));
        assert!(boards.card_drag().is_some(), "travel past 2px did not lift the card");
    }

    #[test]
    fn accepted_drop_moves_optimistically_until_its_generation_snapshot() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        let _ = boards.update(workspace, drag_snapshot(1));
        let drag = drag_state(workspace, 1, 2);

        boards.apply_card_drop(drag.clone());
        assert_eq!(board_card_lane(&boards, workspace), Some(2));
        assert_eq!(
            boards
                .optimistic_drops
                .get(&(workspace, drag.source.id.clone()))
                .map(|drop| { (drop.source_lane, drop.target_lane, drop.generation) }),
            Some((1, 2, None))
        );

        boards.finish_card_drop(
            workspace,
            &drag.source.id,
            &BeadsIssueWriteResult::Applied { generation: 17 },
        );
        assert_eq!(
            boards
                .optimistic_drops
                .get(&(workspace, drag.source.id.clone()))
                .map(|drop| drop.generation),
            Some(Some(17))
        );

        assert!(boards.update(workspace, drag_snapshot(2)).is_empty());
        assert_eq!(board_card_lane(&boards, workspace), Some(2));
        assert!(!boards.optimistic_drops.contains_key(&(workspace, drag.source.id)));
    }

    #[test]
    fn failed_drop_reverts_to_its_source_lane() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        let _ = boards.update(workspace, drag_snapshot(0));
        let drag = drag_state(workspace, 0, 4);
        boards.apply_card_drop(drag.clone());

        boards.finish_card_drop(
            workspace,
            &drag.source.id,
            &BeadsIssueWriteResult::Failed { reason: "bd rejected close".into() },
        );

        assert_eq!(board_card_lane(&boards, workspace), Some(0));
        assert!(!boards.optimistic_drops.contains_key(&(workspace, drag.source.id)));
    }

    #[test]
    fn fence_stale_snapshot_reverts_an_applied_overlay() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        let _ = boards.update(workspace, drag_snapshot(1));
        let drag = drag_state(workspace, 1, 4);
        boards.apply_card_drop(drag.clone());
        boards.finish_card_drop(
            workspace,
            &drag.source.id,
            &BeadsIssueWriteResult::Applied { generation: 19 },
        );

        assert!(boards.update(workspace, drag_snapshot(1)).is_empty());

        assert_eq!(board_card_lane(&boards, workspace), Some(1));
        assert!(!boards.optimistic_drops.contains_key(&(workspace, drag.source.id)));
    }

    #[test]
    fn authoritative_classifier_lane_wins_with_a_notice_outcome() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        let _ = boards.update(workspace, drag_snapshot(1));
        let drag = drag_state(workspace, 1, 4);
        boards.apply_card_drop(drag.clone());
        boards.finish_card_drop(
            workspace,
            &drag.source.id,
            &BeadsIssueWriteResult::Applied { generation: 23 },
        );

        assert_eq!(boards.update(workspace, drag_snapshot(3)), [(drag.source.id.clone(), 3)]);

        assert_eq!(board_card_lane(&boards, workspace), Some(3));
        assert!(!boards.optimistic_drops.contains_key(&(workspace, drag.source.id)));
    }

    #[test]
    fn only_backlog_ready_and_in_progress_cards_can_lift() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();

        for lane in 0..=4 {
            assert_eq!(
                boards.arm_card_drag(workspace, drag_item(), lane, drag_point(150.0, 100.0)),
                lane <= 2,
                "lane {lane} source restriction"
            );
            boards.end_card_drag();
        }
    }

    #[test]
    fn card_drag_tracks_source_pointer_and_lane_or_no_target() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        let item = drag_item();
        boards.arm_card_drag(workspace, item.clone(), 0, drag_point(150.0, 100.0));
        assert!(boards.start_card_drag(drag_point(160.0, 100.0), drag_board()));

        let initial_drag = boards.card_drag().expect("active card drag");
        assert_eq!(initial_drag.workspace_id, workspace);
        assert_eq!(initial_drag.source, item);
        assert_eq!(initial_drag.source_lane, 0);
        assert_eq!(initial_drag.pointer, drag_point(160.0, 100.0));
        assert_eq!(initial_drag.hovered_lane, Some(0));

        assert!(boards.update_card_drag(drag_point(210.0, 120.0), drag_board()));
        let moved_drag = boards.card_drag().expect("updated card drag");
        assert_eq!(moved_drag.pointer, drag_point(210.0, 120.0));
        assert_eq!(moved_drag.hovered_lane, Some(1));

        assert!(boards.update_card_drag(drag_point(600.0, 120.0), drag_board()));
        assert_eq!(boards.card_drag().and_then(|state| state.hovered_lane), None);
        assert!(boards.update_card_drag(drag_point(300.0, 250.0), drag_board()));
        assert_eq!(boards.card_drag().and_then(|state| state.hovered_lane), None);
        assert!(boards.end_card_drag());
        assert!(boards.card_drag().is_none());
    }

    #[test]
    fn card_drag_ghost_exists_only_for_the_active_gesture() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        assert!(boards.card_drag_ghost(drag_board(), 1.0).is_none());

        boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0));
        boards.start_card_drag(drag_point(160.0, 100.0), drag_board());
        let ghost = boards.card_drag_ghost(drag_board(), 1.0).expect("active ghost");
        assert_eq!(ghost.source.id, "scribe-drag.1");
        assert!((ghost.width - 76.8).abs() < 0.001);
        assert!((ghost.height - 46.0).abs() < 0.001);

        boards.end_card_drag();
        assert!(boards.card_drag_ghost(drag_board(), 1.0).is_none());
    }

    #[test]
    fn drag_target_is_read_from_the_existing_board_guard_per_workspace() {
        let workspace = WorkspaceId::new();
        let other = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0));
        boards.start_card_drag(drag_point(210.0, 100.0), drag_board());

        assert_eq!(boards.drag_target(workspace), Some(1));
        assert_eq!(boards.drag_target(other), None);
    }

    #[test]
    fn active_card_drag_owns_pty_pointer_routing() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        assert!(!boards.blocks_pty_mouse());

        boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0));
        assert!(boards.blocks_pty_mouse(), "the press phase must not leak pointer motion");
        boards.start_card_drag(drag_point(160.0, 100.0), drag_board());
        assert!(boards.blocks_pty_mouse());

        boards.end_card_drag();
        assert!(!boards.blocks_pty_mouse());
    }

    #[test]
    fn active_card_drag_holds_a_hover_opened_board_until_release() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.hover(workspace, HoverSource::Board, true);
        boards.hover(workspace, HoverSource::Board, false);
        boards.hover_expires.insert(
            workspace,
            Instant::now().checked_sub(Duration::from_millis(1)).expect("one millisecond fits"),
        );
        boards.arm_card_drag(workspace, drag_item(), 1, drag_point(150.0, 100.0));
        boards.start_card_drag(drag_point(160.0, 100.0), drag_board());

        assert!(!boards.expire_hover());
        assert_eq!(boards.visible(), [(workspace, false)]);
        boards.end_card_drag();
        assert!(boards.expire_hover());
        assert!(boards.visible().is_empty());
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn every_region_pins_hovers_and_closes_its_own_board() {
        let mut ids = [WorkspaceId::new(), WorkspaceId::new(), WorkspaceId::new()];
        ids.sort_by_key(ToString::to_string);
        let [left, middle, right] = ids;
        let mut boards = BeadsBoards::default();

        // Two regions pinned at once, and a third hovered beside them: each
        // region owns a board, so none of these displaces another.
        boards.toggle_pin(left);
        boards.toggle_pin(right);
        boards.hover(middle, HoverSource::Bead, true);
        assert_eq!(
            visible_sorted(&boards),
            [(left, true), (middle, false), (right, true)],
            "focus is not an input, and no board hides another"
        );

        // Hovering a pinned region's bead leaves it pinned rather than
        // downgrading it to a hover that expires.
        boards.hover(left, HoverSource::Bead, true);
        assert!(boards.is_pinned(left));
        assert_eq!(visible_sorted(&boards).into_iter().filter(|(_, p)| *p).count(), 2);

        // Unpinning one leaves the other alone.
        boards.toggle_pin(right);
        assert!(!boards.is_pinned(right));
        assert!(boards.is_pinned(left));

        // Text size is one setting for the window, clamped so the fixed-height
        // strip can still show a readable row.
        assert!((boards.text_scale() - 1.0).abs() < f32::EPSILON);
        boards.adjust_text_scale(-1);
        assert!((boards.text_scale() - 0.9).abs() < 0.001);
        for _ in 0..20 {
            boards.adjust_text_scale(1);
        }
        assert!((boards.text_scale() - 1.6).abs() < 0.001, "grows without bound");
        for _ in 0..20 {
            boards.adjust_text_scale(-1);
        }
        assert!((boards.text_scale() - 0.8).abs() < 0.001, "shrinks without bound");
        boards.adjust_text_scale(2);

        // A region that leaves the window takes its board state with it.
        boards.retain_regions(&HashSet::from([middle]));
        assert_eq!(visible_sorted(&boards), [(middle, false)]);
        // Leaving only starts the grace period the board needs while the
        // pointer crosses onto it, or onto a control inside it.
        boards.hover(middle, HoverSource::Bead, false);
        assert_eq!(visible_sorted(&boards), [(middle, false)]);
        std::thread::sleep(Duration::from_millis(160));
        assert!(boards.expire_hover());
        assert!(boards.visible().is_empty());
    }

    /// A pin outlives the window it was made in: the record names a workspace
    /// the layout has not adopted yet, so the pin waits rather than being
    /// pruned by the reconcile that runs before the region appears.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn restored_pins_wait_for_their_region_to_come_back() {
        let restored = WorkspaceId::new();
        let never_seen = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.restore_pins([restored, never_seen]);

        // The layout is still empty on the first frames after a restart.
        boards.retain_regions(&HashSet::new());
        assert!(boards.visible().is_empty());
        assert!(!boards.is_pinned(restored));

        // The region arrives and takes its pin with it.
        boards.retain_regions(&HashSet::from([restored]));
        assert!(boards.is_pinned(restored));
        assert_eq!(boards.visible(), [(restored, true)]);

        // Pins are handed over once, so unpinning is not undone next frame.
        boards.toggle_pin(restored);
        boards.retain_regions(&HashSet::from([restored]));
        assert!(!boards.is_pinned(restored), "a restored pin came back after being cleared");

        // The order is stable, so a caller diffing against the record sees a
        // change only when one really happened.
        let mut ids = [WorkspaceId::new(), WorkspaceId::new()];
        ids.sort_by_key(WorkspaceId::as_uuid);
        boards.toggle_pin(ids[1]);
        boards.toggle_pin(ids[0]);
        assert_eq!(boards.pinned(), ids);
    }

    /// Dragging one board's bottom bar resizes that board and no other, stays
    /// inside what its region can give, and holds the board open while the
    /// pointer is off it.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn the_bottom_bar_drags_one_boards_height() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.toggle_pin(left);
        boards.toggle_pin(right);
        assert!((boards.height(left) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON);

        // The drag is a delta from where it was grabbed, so a pointer that
        // jumps a frame still lands where the gesture asked.
        boards.start_resize(left, 300.0);
        assert_eq!(boards.resizing(), Some(left));
        assert!(boards.resize_to(360.0, 600.0));
        assert!((boards.height(left) - (BEADS_BOARD_HEIGHT + 60.0)).abs() < 0.001);
        assert!(
            (boards.height(right) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON,
            "resizing one region's board moved another's"
        );

        // The pointer leaving the board it is resizing does not close it.
        boards.hover(left, HoverSource::Board, true);
        boards.hover(left, HoverSource::Board, false);
        std::thread::sleep(Duration::from_millis(160));
        assert!(!boards.expire_hover());

        // Dragging up stops at one readable issue row, and down at what the
        // caller says the region can spare.
        assert!(boards.resize_to(-1000.0, 600.0));
        let floor = boards.height(left);
        assert!(
            floor > beads_board_a2::HEADBAND_H && floor < BEADS_BOARD_HEIGHT,
            "collapsed to {floor}"
        );
        boards.resize_to(4000.0, 240.0);
        assert!((boards.height(left) - 240.0).abs() < 0.001);
        // Even a region with nothing to spare keeps the board readable.
        boards.resize_to(4000.0, 0.0);
        assert!((boards.height(left) - floor).abs() < 0.001);

        assert!(boards.end_resize());
        assert!(!boards.end_resize());
        assert!(!boards.resize_to(500.0, 600.0), "a released bar kept resizing");

        // A region that leaves the window takes its height with it, so the
        // next workspace to land there opens at the designed size.
        boards.retain_regions(&HashSet::from([right]));
        assert!((boards.height(left) - BEADS_BOARD_HEIGHT).abs() < f32::EPSILON);
    }

    /// P0 has to outrank P1 whichever of a theme's two reds is the washed-out
    /// one, and the mark has to rank them even where a reader cannot name the
    /// hue: the wash strengthens as the priority does.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn the_priority_ramp_puts_p0_hottest() {
        // The shape that put P0 below P1 in the field: a bright red that is a
        // pale pink, beside a plain red that is deep.
        let ground = [0.06, 0.07, 0.08, 1.0];
        let mut ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
        ansi[RED] = [0.78, 0.11, 0.13, 1.0];
        ansi[BRIGHT_RED] = [0.93, 0.72, 0.74, 1.0];
        let chrome = ChromeColors {
            tab_bar_bg: ground,
            tab_text: [0.55, 0.56, 0.58, 1.0],
            tab_text_active: [0.9, 0.91, 0.93, 1.0],
            ..chrome_slots(ground)
        };

        let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);

        let hottest = vividness(colors.priorities[0]);
        let below = vividness(colors.priorities[1]);
        assert!(hottest > below, "P0 reads at {hottest:.2} saturation against P1 at {below:.2}");

        // And the wash ranks every step, so a card says which of two issues is
        // hotter before its digits are read. Measured as laid down rather than
        // as a tint, since the tint is solved for exactly this: a hue further
        // from the card makes the same mark with less of itself.
        let laid = |priority: u8| reach(colors.priority_mark(priority).fill, colors.card);
        for priority in 0..4u8 {
            let (strong, weak) = (laid(priority), laid(priority + 1));
            assert!(
                strong > weak,
                "the P{priority} badge carries {strong:.3} against the next rank's {weak:.3}"
            );
        }
    }

    /// A theme whose muted slot and ANSI red sit close to its background must
    /// still produce a board that can be read: the tones are relative to the
    /// mock's ground, and reproducing the ratio there is not legibility here.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn every_text_colour_clears_the_contrast_floor() {
        let ground = [0.06, 0.08, 0.07, 1.0];
        // Barely off the background, the shape that made blockers and ids
        // unreadable in the field.
        let dim = [0.22, 0.24, 0.23, 1.0];
        let dim_red = [0.35, 0.12, 0.12, 1.0];
        let mut ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
        ansi[RED] = dim_red;
        ansi[BRIGHT_RED] = dim_red;
        ansi[BRIGHT_MAGENTA] = [0.3, 0.15, 0.3, 1.0];
        ansi[YELLOW] = [0.42, 0.34, 0.1, 1.0];
        ansi[BRIGHT_YELLOW] = [0.5, 0.42, 0.15, 1.0];
        ansi[BRIGHT_CYAN] = [0.1, 0.3, 0.3, 1.0];
        ansi[BRIGHT_BLUE] = [0.15, 0.15, 0.4, 1.0];
        ansi[BRIGHT_GREEN] = [0.12, 0.3, 0.15, 1.0];
        let chrome = ChromeColors {
            tab_bar_bg: ground,
            tab_text: dim,
            tab_text_active: [0.4, 0.42, 0.41, 1.0],
            tab_separator: [0.3, 0.3, 0.3, 0.3],
            ..chrome_slots(ground)
        };

        let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);

        let ink = Rgba { a: 1.0, ..colors.ground };
        // Words are read on two grounds now — the strip's and the raised
        // card's — and a lift that satisfies one can leave the other short.
        for (name, color) in [
            ("title", colors.title),
            ("queue name", colors.queue_name),
            ("queue name active", colors.queue_name_active),
            ("muted", colors.muted),
            ("quiet", colors.quiet),
            ("P0", colors.priorities[0]),
            ("P1", colors.priorities[1]),
            ("P2", colors.priorities[2]),
            ("P3", colors.priorities[3]),
            ("P4", colors.priorities[4]),
            ("epic", colors.epic),
            ("rank label", colors.rank_label),
            ("agent", colors.agent),
        ] {
            for (surface, under) in [(ink, "the board"), (colors.card, "a card")] {
                let ratio = contrast(color, surface);
                assert!(ratio >= BODY_CONTRAST - 0.01, "{name} reads at {ratio:.2}:1 on {under}");
            }
        }
        // A2's lane label mixes a queue's colour 40% toward the title ink
        // (A2-C2): a mark pulled that far toward text still has to read as
        // text once it lands there.
        for (name, color) in [
            ("backlog", colors.backlog_state),
            ("ready", colors.ready_state),
            ("in progress", colors.progress_state),
            ("blocked", colors.blocked_state),
            ("done", colors.done_state),
        ] {
            let ratio = contrast(mix(color, colors.title, 0.6), ink);
            assert!(ratio >= BODY_CONTRAST - 0.01, "the {name} lane label reads at {ratio:.2}:1");
        }
        for (name, color) in [
            ("chevron", colors.chevron),
            ("backlog", colors.backlog_state),
            ("ready", colors.ready_state),
            ("in progress", colors.progress_state),
            ("blocked", colors.blocked_state),
            ("done", colors.done_state),
        ] {
            let ratio = contrast(color, ink);
            assert!(ratio >= MARK_CONTRAST - 0.01, "{name} mark reads at {ratio:.2}:1");
        }

        // A theme that already reads well keeps its own tones untouched.
        let bright = Rgba { r: 0.9, g: 0.9, b: 0.9, a: 1.0 };
        assert_eq!(readable(bright, ink, BODY_CONTRAST), bright);
    }

    /// The Flow palette carries no colour of its own.
    ///
    /// Its surfaces are the mock's white-alpha overlays, which only say "lift"
    /// on a dark theme, so the derivation follows the ground's polarity rather
    /// than reproducing the mock's tones. Both polarities are checked because
    /// a tint that lightens is invisible on a pale ground.
    // @lat: [[client#Client#Beads Board CLI Data Source#Board interaction and issue detail]]
    #[test]
    fn flow_slots_track_the_theme_on_either_polarity() {
        let themes = [
            ("dark", [0.06, 0.08, 0.07, 1.0], [0.55, 0.57, 0.56, 1.0], [0.4, 0.45, 0.9, 1.0]),
            ("light", [0.95, 0.94, 0.92, 1.0], [0.32, 0.34, 0.33, 1.0], [0.2, 0.25, 0.7, 1.0]),
        ];
        for (name, bg, fg, blue) in themes {
            let mut ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
            ansi[BRIGHT_BLUE] = blue;
            let chrome = ChromeColors {
                tab_bar_bg: bg,
                tab_text: fg,
                tab_text_active: fg,
                ..chrome_slots(bg)
            };

            let colors = BeadsBoardColors::from_theme(&chrome, &ansi, 1.0);

            let ink = Rgba { a: 1.0, ..colors.ground };
            // Words in the graph are read on the same two grounds every other
            // word on the board is.
            let words = [("rank label", colors.rank_label), ("agent", colors.agent)];
            let grounds = [(ink, "the board"), (colors.card, "a card")];
            let read_on = words.into_iter().flat_map(|word| grounds.map(|ground| (word, ground)));
            for ((slot_name, color), (surface, under)) in read_on {
                let ratio = contrast(color, surface);
                assert!(
                    ratio >= BODY_CONTRAST - 0.01,
                    "{slot_name} reads at {ratio:.2}:1 on {under} in the {name} theme"
                );
            }

            // Every surface has to travel away from the ground, whichever
            // direction that is. A fixed white tint passes on a dark theme and
            // paints nothing at all on a pale one.
            let away = |color: Rgba| (luminance(color) - luminance(ink)).abs();
            for (slot_name, color) in [
                ("band", colors.band),
                ("cursor fill", colors.cursor_fill),
                ("wire", colors.wire),
                ("dimmed wire", colors.wire_dimmed),
                ("progress track", colors.progress_track),
                ("chip border", colors.chip_border),
                ("grip", colors.grip),
            ] {
                assert!(away(color) > 0.0, "{slot_name} is invisible on the {name} theme");
            }
            // The graph's own hierarchy, which the trace depends on: a live
            // wire outranks the one it dims to, and the band under-runs both.
            assert!(
                away(colors.wire) > away(colors.wire_dimmed),
                "a {name} traced wire does not out-read the dimmed one"
            );
            assert!(
                away(colors.wire_dimmed) > away(colors.band),
                "the {name} band competes with the wires over it"
            );

            // The two marks that say which run the reader is on take the
            // title's ink, and a chip is the same raised surface as a card.
            assert_eq!(colors.wire_traced, colors.title);
            assert_eq!(colors.cursor_keyline, colors.title);
            assert_eq!(colors.chip, colors.card);
            // Liveness is not the in-progress dot it annotates.
            assert_ne!(
                colors.agent, colors.progress_state,
                "the {name} agent line cannot be told from an in-progress node"
            );
            // The halo is the dot's own hue thinned, so it stays that hue and
            // only its alpha moves. A ring in a second colour would read as
            // another mark beside the dot rather than as the dot glowing.
            assert_eq!(
                (colors.agent_halo.r, colors.agent_halo.g, colors.agent_halo.b),
                (colors.progress_state.r, colors.progress_state.g, colors.progress_state.b),
                "the {name} halo left the progress hue"
            );
            assert!(
                colors.agent_halo.a < colors.progress_state.a,
                "the {name} halo is not thinner than the dot it surrounds"
            );
        }
    }

    /// A theme edit and an opacity edit arrive as separate reload plans, and
    /// the Flow slots have to answer both.
    // @lat: [[client#Client#Beads Board CLI Data Source#Board interaction and issue detail]]
    #[test]
    fn flow_slots_rebuild_from_a_theme_edit_and_an_opacity_edit() {
        let ansi = [[0.5, 0.5, 0.5, 1.0]; 16];
        let dark = chrome_slots([0.06, 0.08, 0.07, 1.0]);
        let light = ChromeColors {
            tab_bar_bg: [0.95, 0.94, 0.92, 1.0],
            tab_text: [0.3, 0.3, 0.3, 1.0],
            tab_text_active: [0.1, 0.1, 0.1, 1.0],
            ..chrome_slots([0.95, 0.94, 0.92, 1.0])
        };

        let base = BeadsBoardColors::from_theme(&dark, &ansi, 1.0);
        let retheme = BeadsBoardColors::from_theme(&light, &ansi, 1.0);
        for (name, before, after) in [
            ("wire", base.wire, retheme.wire),
            ("band", base.band, retheme.band),
            ("chip", base.chip, retheme.chip),
            ("rank label", base.rank_label, retheme.rank_label),
            ("agent", base.agent, retheme.agent),
        ] {
            assert_ne!(before, after, "a theme edit left {name} where it was");
        }

        // Opacity reaches the strip's alpha and stops there: a translucent
        // board must not bleed its graph into the desktop behind the window.
        let faded = BeadsBoardColors::from_theme(&dark, &ansi, 0.5);
        assert!(faded.ground.a < base.ground.a, "an opacity edit never reached the strip");
        assert_eq!(faded.rank_label, base.rank_label);
        assert_eq!(faded.wire, base.wire);

        // The halo is a state mark, so it follows the ANSI ramp the dot comes
        // from rather than the chrome — and it follows it all the way through
        // the contrast lift, which is what keeps ring and dot the same hue on
        // a theme whose blue needs raising.
        let mut vivid = ansi;
        vivid[BRIGHT_BLUE] = [0.15, 0.15, 0.9, 1.0];
        let recoloured = BeadsBoardColors::from_theme(&dark, &vivid, 1.0);
        assert_ne!(recoloured.agent_halo, base.agent_halo, "the halo ignored its own hue");
        assert_eq!(recoloured.agent_halo, alpha(recoloured.progress_state, 0.2));
    }

    /// Liveness is a binding per session, so an ended session clears its own
    /// entry and leaves any other agent on the same issue alone.
    // @lat: [[client#Client#Beads Flow Layout Engine#Reading liveness from a node]]
    #[test]
    fn a_focused_issue_binding_lives_and_dies_with_its_session() {
        let mut boards = BeadsBoards::default();
        let first = SessionId::new();
        let second = SessionId::new();
        assert!(boards.live_issue_ids().is_empty());

        assert!(boards.set_focused_issue(first, Some("flow-2".into())));
        assert_eq!(boards.live_issue_ids(), HashSet::from(["flow-2".to_owned()]));
        assert!(
            !boards.set_focused_issue(first, Some("flow-2".into())),
            "a repeated binding schedules no repaint"
        );

        // Two agents on one issue: the first to leave must not take the halo
        // with it, because the second is still running.
        assert!(boards.set_focused_issue(second, Some("flow-2".into())));
        assert!(boards.set_focused_issue(first, None));
        assert_eq!(boards.live_issue_ids(), HashSet::from(["flow-2".to_owned()]));

        assert!(boards.set_focused_issue(second, None), "session end clears the binding");
        assert!(boards.live_issue_ids().is_empty());
        assert!(!boards.set_focused_issue(second, None), "clearing twice changes nothing");
    }

    /// An agent moving between issues is one binding moving, never two.
    // @lat: [[client#Client#Beads Flow Layout Engine#Reading liveness from a node]]
    #[test]
    fn an_agent_moving_issues_leaves_no_halo_behind() {
        let mut boards = BeadsBoards::default();
        let session = SessionId::new();
        boards.set_focused_issue(session, Some("flow-1".into()));
        assert!(boards.set_focused_issue(session, Some("flow-2".into())));
        assert_eq!(
            boards.live_issue_ids(),
            HashSet::from(["flow-2".to_owned()]),
            "the issue it left is no longer live"
        );
    }

    /// A card cannot reach the window's clipboard, so it parks the text and
    /// the view lifts it on the next frame.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn a_copy_request_is_taken_once() {
        let mut boards = BeadsBoards::default();
        assert_eq!(boards.take_copy(), None);

        boards.copy("nasha-lab-byd.12".to_owned());
        assert_eq!(boards.take_copy().as_deref(), Some("nasha-lab-byd.12"));
        assert_eq!(boards.take_copy(), None, "one click is one copy");
    }

    /// Names taken from the Beads projects on the machine this was written on:
    /// a median epic is one word and 21 characters, and the long tail runs to
    /// 72. Both shapes have to survive — a sentence and a slug.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn short_epic_cuts_long_names_at_a_word_boundary() {
        // Under the cap, nothing happens: half of the real names are here.
        for name in ["dark-mode", "google-auth", "e2e-sandbox-gaps", "Beads integration"] {
            assert_eq!(short_epic(name), name);
        }

        // Sentences break at a space.
        assert_eq!(
            short_epic("Replace hook-derived subagent tracking with snapshots"),
            "Replace hook-derived\u{2026}"
        );
        assert_eq!(
            short_epic("Window resume does not honor screen or minimized state"),
            "Window resume does not\u{2026}"
        );
        // Slugs break at a hyphen.
        assert_eq!(short_epic("client-empty-error-state-parity"), "client-empty-error\u{2026}");
        // A boundary too near the start would throw away more than it saves,
        // so the cut is hard instead.
        assert_eq!(
            short_epic("Antidisestablishmentarianism review"),
            "Antidisestablishmentaria\u{2026}"
        );
        // The ellipsis never lands on a boundary character.
        assert_eq!(
            short_epic("Coordinate legacy - runtime retirement"),
            "Coordinate legacy\u{2026}"
        );
        // Multi-byte names are cut by character, not by byte.
        assert_eq!(short_epic(&"é".repeat(40)), format!("{}\u{2026}", "é".repeat(24)));
    }

    /// The card shows the issue, not the project: every card on a board
    /// carries the same prefix, and the space is the title's.
    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn short_id_drops_the_project_and_nothing_else() {
        assert_eq!(short_id("nasha-lab-byd.12"), "byd.12");
        assert_eq!(short_id("scribe-3j2y"), "3j2y");
        assert_eq!(short_id("sc-70"), "70");
        // A child issue keeps the parent it hangs off.
        assert_eq!(short_id("scribe-aq1.23"), "aq1.23");
        // Nothing to drop, or nothing left after dropping: keep the whole id,
        // because half an id is worse than a long one.
        assert_eq!(short_id("70"), "70");
        assert_eq!(short_id("scribe-"), "scribe-");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn fresh_ready_snapshot_does_not_refresh_on_hover() {
        let workspace = WorkspaceId::new();
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis()
            .try_into()
            .expect("epoch fits");
        let mut boards = BeadsBoards::default();
        let _ = boards.update(
            workspace,
            BeadsBoardState::Ready {
                snapshot: BeadsBoardSnapshot { refreshed_at_epoch_ms: now, ..Default::default() },
                stale: false,
                refresh_error: None,
            },
        );

        assert!(!boards.needs_refresh(workspace, Duration::from_secs(30)));
    }

    #[test]
    fn unavailable_board_schedules_one_retry_per_interval() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.toggle_pin(workspace);
        let _ =
            boards.update(workspace, BeadsBoardState::Unavailable { message: "missing bd".into() });

        assert!(boards.is_pinned(workspace), "a temporary backend failure closed the board");
        assert_eq!(boards.due_retry(Duration::from_secs(30)), Some(workspace));
        assert_eq!(boards.due_retry(Duration::from_secs(30)), None);
    }

    // @lat: [[client#Client#Beads Board CLI Data Source#Board interaction and issue detail]]
    #[test]
    fn not_detected_closes_only_that_workspaces_board_and_gestures() {
        let missing = WorkspaceId::new();
        let neighbour = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.toggle_pin(missing);
        boards.hover(missing, HoverSource::Board, true);
        boards.hover(missing, HoverSource::Board, false);
        boards.toggle_pin(neighbour);
        boards.hover(neighbour, HoverSource::Board, true);
        boards.start_resize(missing, 100.0);
        boards.arm_card_drag(missing, drag_item(), 1, drag_point(150.0, 100.0));

        let _ = boards.update(missing, BeadsBoardState::NotDetected);

        assert_eq!(visible_sorted(&boards), [(neighbour, true)]);
        assert!(!boards.hover_expires.contains_key(&missing));
        assert_eq!(boards.resizing(), None);
        assert!(!boards.blocks_pty_mouse());
        assert!(boards.is_pinned(neighbour));
        assert_eq!(boards.hovered.get(&neighbour), Some(&(HoverSource::Board as u8)));

        boards.toggle_pin(missing);
        boards.start_resize(missing, 100.0);
        boards.arm_card_drag(missing, drag_item(), 1, drag_point(150.0, 100.0));
        assert!(boards.start_card_drag(drag_point(160.0, 100.0), drag_board()));

        let _ = boards.update(missing, BeadsBoardState::NotDetected);

        assert_eq!(boards.resizing(), None);
        assert!(!boards.blocks_pty_mouse());
        assert!(boards.card_drag().is_none());
        assert_eq!(visible_sorted(&boards), [(neighbour, true)]);
    }
}

#[cfg(test)]
mod flow_mode_tests {
    use scribe_common::protocol::{
        BeadsEpicGraph, BeadsEpicGraphOutcome, BeadsEpicGraphRefusal, BeadsGraphEdge,
        BeadsGraphNode, BeadsIssueQueue,
    };

    use super::*;

    const EPIC: &str = "flow-epic";

    fn card(id: &str, epic: Option<&str>) -> BeadsBoardItem {
        BeadsBoardItem {
            id: id.into(),
            title: format!("Card {id}"),
            priority: 1,
            blocker_ids: Vec::new(),
            parent_epic_name: epic.map(|_| "Flow epic".into()),
            parent_epic_id: epic.map(str::to_owned),
            updated_at: String::new(),
        }
    }

    fn node(id: &str) -> BeadsGraphNode {
        BeadsGraphNode {
            id: id.into(),
            title: format!("Node {id}"),
            priority: 1,
            status: "open".into(),
            queue: BeadsIssueQueue::Ready,
            assignee: None,
            updated_at: "2026-08-18T00:00:00Z".into(),
        }
    }

    fn graph() -> BeadsEpicGraphOutcome {
        BeadsEpicGraphOutcome::Graph(Box::new(BeadsEpicGraph {
            epic_id: EPIC.into(),
            epic_title: "Flow epic".into(),
            closed: 0,
            total: 3,
            nodes: vec![node("a"), node("b"), node("c")],
            edges: vec![
                BeadsGraphEdge { from: "a".into(), to: "b".into() },
                BeadsGraphEdge { from: "b".into(), to: "c".into() },
            ],
        }))
    }

    fn enabled() -> BeadsBoards {
        let mut boards = BeadsBoards::default();
        boards.set_flow_enabled(true);
        boards
    }

    #[test]
    fn a_card_naming_an_epic_asks_for_its_graph_and_opens_flow() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));

        assert_eq!(boards.take_flow_request(), Some((workspace, EPIC.to_owned())));
        assert!(boards.flow(workspace).is_none(), "no strip before the reply lands");
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        let flow = boards.flow(workspace).expect("flow open");
        assert_eq!(flow.cursor_issue_id, "a");
        assert_eq!(flow.epic_id, EPIC);
        assert!((flow.scroll_x - 0.0).abs() < f32::EPSILON);
    }

    /// The regression guard for the deadlock that blanked the whole board.
    ///
    /// `render` is called with the board guard still held, so anything the
    /// paint path needs must already be owned by the time it starts. This
    /// pins that shape from inside a held guard: `try_lock` proving the mutex
    /// is unavailable is the same condition the renderer runs under, and a
    /// paint-time `lock()` there would not fail, it would hang forever.
    #[test]
    fn the_flow_paint_input_is_read_under_the_guard_the_renderer_runs_inside() {
        let workspace = WorkspaceId::new();
        let mut seeded = enabled();
        seeded.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(seeded.apply_epic_graph(workspace, EPIC, graph()));
        let shared = std::sync::Arc::new(std::sync::Mutex::new(seeded));

        let guard = shared.lock().expect("render pass takes the board guard");
        assert!(
            shared.try_lock().is_err(),
            "the renderer paints inside this guard, so any second lock deadlocks"
        );
        let snapshot = guard.flow_snapshot(workspace).expect("a strip in Flow paints a snapshot");
        drop(guard);

        assert_eq!(snapshot.cursor_issue_id, "a");
        assert_eq!(snapshot.graph.nodes.len(), 3);
        assert!(snapshot.trace.is_none(), "an untouched graph paints at rest");
    }

    /// Lanes paint through the same call, and the old lookup locked before it
    /// tested for Flow — which is why a lanes-only board went blank too.
    #[test]
    fn a_board_in_lanes_needs_no_flow_snapshot_and_no_second_lock() {
        let workspace = WorkspaceId::new();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(enabled()));

        let guard = shared.lock().expect("render pass takes the board guard");
        assert!(shared.try_lock().is_err());
        assert!(guard.flow_snapshot(workspace).is_none(), "lanes carry no Flow snapshot");
    }

    #[test]
    fn a_card_with_no_epic_stays_in_lanes_without_a_request() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("loose", None));

        assert_eq!(boards.take_flow_request(), None);
        assert!(boards.flow(workspace).is_none());
    }

    #[test]
    fn a_refused_epic_stays_in_lanes() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));

        let refused = BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::Cycle };
        assert!(!boards.apply_epic_graph(workspace, EPIC, refused));
        assert!(boards.flow(workspace).is_none());
        // The fence is spent, so the refusal cannot be retried by a late graph.
        assert!(!boards.apply_epic_graph(workspace, EPIC, graph()));
    }

    #[test]
    fn two_clicks_on_one_epic_land_on_the_second_however_replies_arrive() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        boards.request_card_flow(workspace, &card("c", Some(EPIC)));

        // The reply carries graph content only, so the first one home opens at
        // the latest click rather than resurrecting its own stale target.
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        assert_eq!(boards.flow(workspace).expect("flow open").cursor_issue_id, "c");

        // Its twin is spent against the same fence and changes nothing.
        assert!(!boards.apply_epic_graph(workspace, EPIC, graph()));
        assert_eq!(boards.flow(workspace).expect("flow open").cursor_issue_id, "c");
    }

    #[test]
    fn hovering_a_node_sets_the_trace_and_leaving_it_clears() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        assert!(boards.set_flow_hover(workspace, "b", true));
        assert_eq!(boards.flow(workspace).unwrap().hovered_issue_id.as_deref(), Some("b"));
        // Re-entering the node already traced repaints nothing.
        assert!(!boards.set_flow_hover(workspace, "b", true));

        assert!(boards.set_flow_hover(workspace, "b", false));
        assert_eq!(boards.flow(workspace).unwrap().hovered_issue_id, None);
    }

    #[test]
    fn a_stale_leave_cannot_erase_the_trace_the_next_node_just_set() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        // Pointers cross borders in an arbitrary order: the enter for the new
        // node can land before the leave for the old one.
        assert!(boards.set_flow_hover(workspace, "a", true));
        assert!(boards.set_flow_hover(workspace, "b", true));
        assert!(!boards.set_flow_hover(workspace, "a", false));
        assert_eq!(boards.flow(workspace).unwrap().hovered_issue_id.as_deref(), Some("b"));
    }

    #[test]
    fn hover_is_refused_outside_the_graph_and_outside_flow() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        assert!(!boards.set_flow_hover(workspace, "a", true), "no flow open");

        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        assert!(!boards.set_flow_hover(workspace, "not-in-graph", true));
        assert_eq!(boards.flow(workspace).unwrap().hovered_issue_id, None);
    }

    #[test]
    fn a_reopened_graph_starts_untraced() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        assert!(boards.set_flow_hover(workspace, "b", true));

        boards.exit_flow(workspace);
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        assert_eq!(boards.flow(workspace).unwrap().hovered_issue_id, None);
    }

    #[test]
    fn a_reply_arriving_after_exit_is_discarded() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        boards.exit_flow(workspace);

        assert!(!boards.apply_epic_graph(workspace, EPIC, graph()));
        assert!(boards.flow(workspace).is_none());
    }

    #[test]
    fn a_reply_for_another_epic_is_discarded() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));

        assert!(!boards.apply_epic_graph(workspace, "other-epic", graph()));
        assert!(boards.flow(workspace).is_none());
    }

    #[test]
    fn flow_needs_the_capability_and_dies_when_it_is_lost() {
        let workspace = WorkspaceId::new();
        let mut ungated = BeadsBoards::default();
        ungated.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert_eq!(ungated.take_flow_request(), None, "no capability, no request");

        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        boards.set_flow_enabled(false);
        assert!(boards.flow(workspace).is_none(), "a lost capability drops the strip");
    }

    #[test]
    fn escape_returns_the_latest_strip_to_lanes() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        boards.apply_epic_graph(workspace, EPIC, graph());

        assert!(boards.exit_latest_flow());
        assert!(boards.flow(workspace).is_none());
        assert!(!boards.exit_latest_flow(), "nothing left to leave");
    }

    #[test]
    fn escape_exits_the_most_recently_opened_strip_not_the_largest_uuid() {
        let first = WorkspaceId::new();
        let second = WorkspaceId::new();
        let (largest_uuid, smallest_uuid) =
            if first.as_uuid() > second.as_uuid() { (first, second) } else { (second, first) };
        let mut boards = enabled();
        for workspace in [largest_uuid, smallest_uuid] {
            boards.request_card_flow(workspace, &card("a", Some(EPIC)));
            assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        }

        assert!(boards.exit_latest_flow());
        assert!(boards.flow(smallest_uuid).is_none(), "Escape did not exit the latest Flow");
        assert!(boards.flow(largest_uuid).is_some(), "Escape selected by UUID instead");
    }

    #[test]
    fn one_region_leaving_flow_leaves_its_neighbour_alone() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = enabled();
        for workspace in [left, right] {
            boards.request_card_flow(workspace, &card("a", Some(EPIC)));
            assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        }

        boards.exit_flow(left);
        assert!(boards.flow(left).is_none());
        assert!(boards.flow(right).is_some(), "a board is its own region's furniture");
    }

    #[test]
    fn losing_a_region_drops_its_strip_and_its_request() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(left, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(left, EPIC, graph()));
        boards.request_card_flow(right, &card("a", Some(EPIC)));

        boards.retain_regions(&HashSet::new());
        assert!(boards.flow(left).is_none());
        assert!(!boards.apply_epic_graph(right, EPIC, graph()));
        assert_eq!(boards.take_flow_request(), None);
    }

    #[test]
    fn a_not_detected_snapshot_returns_the_board_to_lanes() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        boards.update(workspace, BeadsBoardState::NotDetected);
        assert!(boards.flow(workspace).is_none());
    }

    #[test]
    fn the_wheel_scrolls_one_axis_and_clamps_at_both_ends() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        let narrow = Rect { x: 0.0, y: 0.0, width: 120.0, height: 197.0 };

        assert!(boards.scroll_flow(workspace, 40.0, narrow));
        assert!(boards.flow(workspace).expect("flow open").scroll_x > 0.0);

        // Past either end the offset stops moving rather than running off.
        while boards.scroll_flow(workspace, 500.0, narrow) {}
        let span = boards.flow(workspace).expect("flow open").scroll_x;
        assert!(!boards.scroll_flow(workspace, 500.0, narrow), "clamped at the far end");
        assert!(span > 0.0);

        while boards.scroll_flow(workspace, -500.0, narrow) {}
        assert!((boards.flow(workspace).expect("flow open").scroll_x - 0.0).abs() < f32::EPSILON);
        assert!(!boards.scroll_flow(workspace, -500.0, narrow), "clamped at the near end");
    }

    #[test]
    fn a_graph_narrower_than_its_strip_never_scrolls() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        let wide = Rect { x: 0.0, y: 0.0, width: 4000.0, height: 197.0 };

        assert!(!boards.scroll_flow(workspace, 200.0, wide));
    }

    #[test]
    fn activating_a_node_moves_the_cursor_inside_the_frozen_graph() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        let opened = boards.flow(workspace).expect("flow open").graph.clone();

        let laid_out = boards.flow(workspace).expect("flow open").layout.clone();
        assert_eq!(boards.take_flow_request(), Some((workspace, EPIC.to_owned())));
        assert_eq!(boards.take_flow_request(), None, "the opening request is spent");

        assert!(boards.move_flow_cursor(workspace, "c"));
        let flow = boards.flow(workspace).expect("flow open");
        assert_eq!(flow.cursor_issue_id, "c");
        assert_eq!(flow.epic_id, EPIC, "the epic never swaps under a node click");
        assert_eq!(flow.graph, opened, "the graph is frozen at open");
        assert_eq!(flow.layout, laid_out, "every rank and wire survives the click");
        assert_eq!(
            boards.take_flow_request(),
            None,
            "moving the cursor must not ask for a second graph"
        );

        assert!(!boards.move_flow_cursor(workspace, "c"), "re-clicking the cursor is a no-op");
        assert!(!boards.move_flow_cursor(workspace, "absent"));
        assert_eq!(boards.take_flow_request(), None, "a refused activation sends nothing");
        let settled = boards.flow(workspace).expect("flow open");
        assert_eq!(settled.cursor_issue_id, "c", "a refused activation leaves the cursor put");
        assert_eq!(settled.layout, laid_out);
    }

    #[test]
    fn text_scale_survives_the_round_trip_and_relayouts_an_open_strip() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.adjust_text_scale(-2);
        let scaled = boards.text_scale();
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));
        let narrow = boards.flow(workspace).expect("flow open").layout.width;

        boards.adjust_text_scale(2);
        let widened = boards.flow(workspace).expect("flow open").layout.width;
        assert!(widened > narrow, "a bigger text scale re-lays the open strip out");

        boards.exit_flow(workspace);
        boards.adjust_text_scale(-2);
        assert!((boards.text_scale() - scaled).abs() < f32::EPSILON);
    }

    #[test]
    fn pin_and_height_survive_the_round_trip() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.update(workspace, BeadsBoardState::Loading { cached: None });
        boards.toggle_pin(workspace);
        // Drag the strip off its default height first: a preserved height and a
        // height reset to the default are indistinguishable otherwise.
        boards.start_resize(workspace, 100.0);
        boards.resize_to(160.0, 400.0);
        let pinned_height = boards.height(workspace);
        assert!(
            (pinned_height - BEADS_BOARD_HEIGHT).abs() > f32::EPSILON,
            "the resize must move the height off its default"
        );
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        assert!(boards.is_pinned(workspace), "Flow paints inside the strip the pin reserved");
        assert!((boards.height(workspace) - pinned_height).abs() < f32::EPSILON);

        boards.exit_flow(workspace);
        assert!(boards.is_pinned(workspace));
        assert!((boards.height(workspace) - pinned_height).abs() < f32::EPSILON);
    }

    /// A2-L2: the lane pin is board furniture, not Flow state, so it must
    /// survive the strip's own round trip exactly like the board pin does.
    #[test]
    fn lane_pin_survives_the_flow_round_trip() {
        let workspace = WorkspaceId::new();
        let mut boards = enabled();
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);
        boards.request_card_flow(workspace, &card("a", Some(EPIC)));
        assert!(boards.apply_epic_graph(workspace, EPIC, graph()));

        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned),
            "Flow paints over lanes, not over the lane pin itself"
        );

        boards.exit_flow(workspace);
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );
    }
}

/// Unit coverage for the A2 collapsed-lane hover/pin state owned by
/// `BeadsBoards` — scribe-zwtv.5. `specs/028-beads-board-contract.md` is
/// normative; these tests pin its A2-I1, A2-I2, and A2-L1 rows, plus the
/// closed pinned-lane, lifetime, and keyboard-equivalent decisions.
#[cfg(test)]
mod lane_tests {
    use scribe_common::protocol::BeadsIssueQueue;

    use super::*;

    const NON_COLLAPSIBLE: [BeadsIssueQueue; 3] =
        [BeadsIssueQueue::Backlog, BeadsIssueQueue::Ready, BeadsIssueQueue::InProgress];

    #[test]
    fn collapsed_lanes_default_to_a_tab() {
        let workspace = WorkspaceId::new();
        let boards = BeadsBoards::default();

        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );
    }

    #[test]
    fn only_blocked_and_done_are_collapsible_queues() {
        let workspace = WorkspaceId::new();
        for queue in NON_COLLAPSIBLE {
            let mut boards = BeadsBoards::default();
            assert_eq!(boards.collapsed_lane_state(workspace, queue), None);
            assert!(!boards.pin_lane(workspace, queue));
            assert!(!boards.hover_lane(workspace, queue, LaneHoverSource::Tab, true));
            assert!(!boards.unpin_lane(workspace, queue));
            assert!(!boards.close_lane_drawer(workspace, queue));
            assert_eq!(
                boards.collapsed_lane_state(workspace, queue),
                None,
                "a rejected queue must not be coerced into a real one"
            );
        }
    }

    #[test]
    fn hover_or_focus_opens_the_drawer() {
        let workspace = WorkspaceId::new();

        let mut hovered = BeadsBoards::default();
        assert!(hovered.hover_lane(
            workspace,
            BeadsIssueQueue::Blocked,
            LaneHoverSource::Tab,
            true
        ));
        assert_eq!(
            hovered.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Open)
        );

        let mut focused = BeadsBoards::default();
        assert!(focused.hover_lane(workspace, BeadsIssueQueue::Done, LaneHoverSource::Focus, true));
        assert_eq!(
            focused.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Open)
        );
    }

    #[test]
    fn grace_period_transfers_from_tab_to_drawer() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, true);
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Drawer, true);

        // Leaving the tab for the drawer it opened must not start closing it.
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, false);
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Open)
        );
        assert!(!boards.expire_hover());

        // Leaving the drawer too starts the same grace the board itself gets.
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Drawer, false);
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Open),
            "still open inside its grace period"
        );
        assert!(!boards.expire_hover());

        boards.lane_hover_expires.insert(
            workspace,
            Instant::now().checked_sub(Duration::from_millis(1)).expect("one millisecond fits"),
        );
        assert!(boards.expire_hover());
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
    }

    #[test]
    fn entering_a_different_queue_replaces_the_open_drawer() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, true);

        assert!(boards.hover_lane(workspace, BeadsIssueQueue::Done, LaneHoverSource::Tab, true));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab),
            "only one drawer is ever open per workspace"
        );
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Open)
        );

        // The abandoned queue's stale leave is ignored rather than closing
        // the drawer that replaced it.
        assert!(!boards.hover_lane(
            workspace,
            BeadsIssueQueue::Blocked,
            LaneHoverSource::Tab,
            false
        ));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Open)
        );
    }

    #[test]
    fn pin_lane_replaces_whichever_queue_was_pinned() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();

        assert!(boards.pin_lane(workspace, BeadsIssueQueue::Blocked));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );

        // Pinning Done unpins Blocked instead of both lanes staying pinned:
        // A2 never shows a two-pinned layout.
        assert!(boards.pin_lane(workspace, BeadsIssueQueue::Done));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Pinned)
        );
        assert_eq!(boards.lane_pinned(), [(workspace, BeadsIssueQueue::Done)]);
    }

    #[test]
    fn unpin_lane_restores_the_tab() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);

        assert!(!boards.unpin_lane(workspace, BeadsIssueQueue::Done), "Done was never pinned");
        assert!(boards.unpin_lane(workspace, BeadsIssueQueue::Blocked));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
        assert!(boards.lane_pinned().is_empty());
        assert!(!boards.unpin_lane(workspace, BeadsIssueQueue::Blocked), "already unpinned");
    }

    #[test]
    fn a_pinned_lane_ignores_its_own_stale_hover_state() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, true);
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);

        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );

        // Unpinning must not resurrect the tab-hover bit pinning left behind.
        boards.unpin_lane(workspace, BeadsIssueQueue::Blocked);
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab),
            "a stale hover bit leaked through the pin"
        );
    }

    #[test]
    fn hover_expiry_never_unpins_a_pinned_lane() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);
        boards.lane_hovered.insert(workspace, (BeadsIssueQueue::Blocked, 0));
        boards.lane_hover_expires.insert(
            workspace,
            Instant::now().checked_sub(Duration::from_millis(1)).expect("one millisecond fits"),
        );

        boards.expire_hover();
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned),
            "hover expiry must never clear a pin"
        );
    }

    #[test]
    fn sibling_workspaces_pin_and_hover_their_own_queues() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = BeadsBoards::default();

        boards.pin_lane(left, BeadsIssueQueue::Blocked);
        boards.hover_lane(right, BeadsIssueQueue::Done, LaneHoverSource::Tab, true);

        assert_eq!(
            boards.collapsed_lane_state(left, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );
        assert_eq!(
            boards.collapsed_lane_state(left, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(right, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Open)
        );
        assert_eq!(
            boards.collapsed_lane_state(right, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
    }

    #[test]
    fn losing_a_region_drops_its_lane_pin_and_hover() {
        let missing = WorkspaceId::new();
        let neighbour = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(missing, BeadsIssueQueue::Blocked);
        boards.hover_lane(neighbour, BeadsIssueQueue::Done, LaneHoverSource::Tab, true);

        boards.retain_regions(&HashSet::from([neighbour]));

        assert_eq!(
            boards.collapsed_lane_state(missing, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
        assert!(boards.lane_pinned().is_empty());
        assert_eq!(
            boards.collapsed_lane_state(neighbour, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Open),
            "an unrelated region keeps its own state"
        );
    }

    // @lat: [[client#Client#Beads Board CLI Data Source#Board interaction and issue detail]]
    #[test]
    fn not_detected_clears_only_that_workspaces_lane_state() {
        let missing = WorkspaceId::new();
        let neighbour = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(missing, BeadsIssueQueue::Blocked);
        boards.hover_lane(missing, BeadsIssueQueue::Done, LaneHoverSource::Tab, true);
        boards.pin_lane(neighbour, BeadsIssueQueue::Done);

        let _ = boards.update(missing, BeadsBoardState::NotDetected);

        assert_eq!(
            boards.collapsed_lane_state(missing, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(missing, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(neighbour, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Pinned),
            "a neighbouring workspace's pin must survive"
        );
    }

    #[test]
    fn lane_pin_survives_a_flow_capability_change() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);

        boards.set_flow_enabled(true);
        boards.set_flow_enabled(false);

        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned),
            "losing beads_flow must not touch the lane pin"
        );
    }

    /// A lane pin outlives the window it was made in, the same way a board
    /// pin does: the record names a workspace the layout has not adopted
    /// yet, so it waits rather than being pruned before the region appears.
    #[test]
    fn restored_lane_pins_wait_for_their_region_to_come_back() {
        let restored = WorkspaceId::new();
        let never_seen = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.restore_lane_pins([
            (restored, BeadsIssueQueue::Blocked),
            (never_seen, BeadsIssueQueue::Done),
        ]);

        // The layout is still empty on the first frames after a restart.
        boards.retain_regions(&HashSet::new());
        assert_eq!(
            boards.collapsed_lane_state(restored, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab)
        );

        // The region arrives and takes its pin with it.
        boards.retain_regions(&HashSet::from([restored]));
        assert_eq!(
            boards.collapsed_lane_state(restored, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );
        assert_eq!(boards.lane_pinned(), [(restored, BeadsIssueQueue::Blocked)]);

        // Pins are handed over once, so an explicit unpin is not undone next
        // frame.
        boards.unpin_lane(restored, BeadsIssueQueue::Blocked);
        boards.retain_regions(&HashSet::from([restored]));
        assert!(boards.lane_pinned().is_empty(), "a restored pin came back after being cleared");
    }

    #[test]
    fn restore_lane_pins_rejects_a_persisted_non_collapsible_queue() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.restore_lane_pins([(workspace, BeadsIssueQueue::Ready)]);

        boards.retain_regions(&HashSet::from([workspace]));

        assert!(boards.lane_pinned().is_empty(), "an invalid persisted queue must not be coerced");
    }

    #[test]
    fn repeated_hover_and_pin_events_are_idempotent() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();

        assert!(boards.hover_lane(workspace, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, true));
        assert!(!boards.hover_lane(
            workspace,
            BeadsIssueQueue::Blocked,
            LaneHoverSource::Tab,
            true
        ));

        assert!(boards.pin_lane(workspace, BeadsIssueQueue::Blocked));
        assert!(!boards.pin_lane(workspace, BeadsIssueQueue::Blocked));

        assert!(boards.unpin_lane(workspace, BeadsIssueQueue::Blocked));
        assert!(!boards.unpin_lane(workspace, BeadsIssueQueue::Blocked));
    }

    #[test]
    fn escape_closes_only_a_transient_drawer() {
        let workspace = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        boards.pin_lane(workspace, BeadsIssueQueue::Blocked);
        boards.hover_lane(workspace, BeadsIssueQueue::Done, LaneHoverSource::Focus, true);

        assert!(
            !boards.close_lane_drawer(workspace, BeadsIssueQueue::Blocked),
            "Escape never touches a pin"
        );
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned)
        );

        assert!(boards.close_lane_drawer(workspace, BeadsIssueQueue::Done));
        assert_eq!(
            boards.collapsed_lane_state(workspace, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );
        assert!(!boards.expire_hover(), "an explicit close needs no grace to expire");
    }

    #[test]
    fn close_any_lane_drawer_closes_every_open_transient_drawer_but_not_a_pin() {
        let left = WorkspaceId::new();
        let right = WorkspaceId::new();
        let mut boards = BeadsBoards::default();
        assert!(!boards.close_any_lane_drawer(), "nothing open yet");

        boards.pin_lane(left, BeadsIssueQueue::Blocked);
        boards.hover_lane(left, BeadsIssueQueue::Done, LaneHoverSource::Focus, true);
        boards.hover_lane(right, BeadsIssueQueue::Blocked, LaneHoverSource::Tab, true);

        assert!(boards.close_any_lane_drawer());
        assert_eq!(
            boards.collapsed_lane_state(left, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Pinned),
            "Escape never touches a pin"
        );
        assert_eq!(
            boards.collapsed_lane_state(left, BeadsIssueQueue::Done),
            Some(CollapsedLaneState::Tab)
        );
        assert_eq!(
            boards.collapsed_lane_state(right, BeadsIssueQueue::Blocked),
            Some(CollapsedLaneState::Tab),
            "a second region's open drawer closes too"
        );
        assert!(!boards.close_any_lane_drawer(), "already closed");
    }
}
