//! The running client's live pane and workspace layout.
//!
//! This is the seam that turns the ported split-tree models into something the
//! shipped binary reaches. The window owns exactly one
//! [`WorkspaceTree`] — the window-level split of the grid area into workspace
//! regions — and one [`PaneTree`] per region, which splits that region into
//! panes. Every pane hosts at most one session, and the focused pane of the
//! focused region is the pane keystrokes, the status bar, and the tab strip
//! follow.
//!
//! Two layers rather than one, because Scribe's chrome has always had two: a
//! workspace region is a project-scoped column of the window with its own
//! accent colour ([`WorkspaceSlot::accent_color`]), and panes are the splits a
//! user makes inside one. `workspace_split_*` moves the outer divider,
//! `split_*` the inner one, and the two focus families move within their own
//! layer.
//!
//! The shell holds no pixel state. Callers hand it a viewport rect (the grid
//! area expressed in the window's nominal cell metrics) whenever geometry
//! matters — directional focus and per-pane sizing — so the layout stays a pure
//! function of the trees plus the live font metrics.

use std::collections::{HashMap, HashSet, VecDeque};

use gpui::{App, AppContext as _, Entity};
use scribe_client::divider::{self, Divider};
use scribe_client::layout::{FocusDirection, LayoutNode, LayoutTree, PaneId, Rect, SplitDirection};
use scribe_client::pane_tree::PaneTree;
use scribe_client::prompt_bar::PromptBarData;
use scribe_client::restore_replay::{
    PaneRestore, RebuiltWindow, ReplayLaunch, new_shell_binding, snapshot_window_restore,
};
use scribe_client::restore_state::{LaunchBinding, WindowRestoreState};
use scribe_client::tab_session::TabSessions;
use scribe_client::workspace_layout::{
    WindowLayout, WorkspaceDivider, WorkspaceSlot, pane_tree_to_layout_node,
};
use scribe_client::workspace_tree::WorkspaceTree;
use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{LayoutDirection, PaneTreeNode, WorkspaceTreeNode},
};

/// Accent used when a region's slot cannot be read back out of the layout,
/// which only happens if a region is removed between two reads in one frame.
const FALLBACK_PANE_ACCENT: [f32; 4] = [0.0, 0.8, 0.7, 1.0];

/// Height of the tab bar a lower workspace region reserves at its top edge,
/// matching [`scribe_client::titlebar::TITLEBAR_HEIGHT`] so stacked regions
/// read as the same chrome. Regions on the window's top row keep their tabs in
/// the titlebar and reserve nothing.
pub const REGION_TAB_BAR_HEIGHT: f32 = scribe_client::titlebar::TITLEBAR_HEIGHT;

/// One leaf pane, resolved against a viewport for a single frame.
#[derive(Debug, Clone, Copy)]
pub struct PanePlacement {
    /// The workspace region this pane belongs to.
    pub workspace_id: WorkspaceId,
    /// The pane's identity within its region's split tree.
    pub pane_id: PaneId,
    /// The session the pane is showing, or `None` while a split waits for the
    /// server to answer with `SessionCreated`.
    pub session_id: Option<SessionId>,
    /// The pane's rect inside the viewport it was computed against.
    pub rect: Rect,
    /// Whether this is the focused pane of the focused region.
    pub focused: bool,
    /// The owning region's accent colour, used to tint the focus ring so two
    /// regions are visually distinguishable.
    pub accent: [f32; 4],
}

/// What [`PaneShell::close_focused_pane`] actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosedPane {
    /// A pane (or a whole region) went away.
    Removed {
        /// Sessions that lost their pane and must be closed on the server.
        sessions: Vec<SessionId>,
        /// The workspace region that collapsed with the pane, when it was one
        /// the server had minted. `None` when only a pane inside a surviving
        /// region went away, or when the collapsed region was still waiting for
        /// its `WorkspaceInfo` and so names an id the server never saw.
        closed_region: Option<WorkspaceId>,
    },
    /// The window is down to one pane in one region, so there is nothing to
    /// close at the layout level; the caller falls back to closing the tab.
    LastPane,
}

/// Server-owned workspace metadata delivered by `WorkspaceInfo` or
/// `WorkspaceNamed`.
///
/// Kept as one value so the reader can park a whole update and the GPUI thread
/// can apply it to the region's slot in a single call, rather than threading
/// four parallel arguments through the shell.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceInfo {
    /// The workspace the server is describing.
    pub workspace_id: WorkspaceId,
    /// Display name, or `None` while the workspace is outside any configured
    /// project root.
    pub name: Option<String>,
    /// Accent colour from the server's rotating palette, already parsed out of
    /// its `#rrggbb` wire form.
    pub accent: Option<[f32; 4]>,
    /// Project directory the workspace was derived from.
    pub project_root: Option<std::path::PathBuf>,
}

/// What one drained [`WorkspaceInfo`] did to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceInfoOutcome {
    /// The id was already in the layout; only the slot's metadata changed.
    Updated,
    /// A region that was waiting for a server workspace adopted this id.
    Adopted,
    /// No region claimed the id: the window does not show this workspace.
    Unclaimed,
}

/// What one call to `PaneShell::retire_pane` removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retired {
    /// A pane went away; the payload names the server-minted region that
    /// collapsed with it, if any.
    Pane(Option<WorkspaceId>),
    /// Nothing could be removed — one pane in one region.
    Nothing,
}

/// The result of dropping panes whose sessions have exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetiredPanes {
    /// Whether the layout actually changed, so the caller republishes sizes.
    pub changed: bool,
    /// Server-minted regions that collapsed because their last pane went away.
    pub closed_regions: Vec<WorkspaceId>,
}

/// The scratch layout a restore snapshot is assembled into, plus the per-pane
/// records that go with it.
///
/// The two always travel together — a pane record is keyed by the id the layout
/// minted for it — so they are one parameter rather than two.
struct SnapshotTarget<'a> {
    layout: &'a mut WindowLayout,
    panes: &'a mut HashMap<PaneId, PaneRestore>,
}

/// The live per-session state a restore snapshot reads, keyed by session id.
///
/// Neither map lives on the shell — launch bindings sit on the restore runtime
/// and prompt history in the IPC-written `AiChrome` — so both are handed in
/// together rather than looked up per pane.
struct SnapshotSources<'a> {
    bindings: &'a HashMap<SessionId, LaunchBinding>,
    prompts: &'a HashMap<SessionId, PromptBarData>,
}

/// Build the slot edit that reapplies a restored region's name and accent.
fn apply_slot_metadata(name: Option<String>, accent: [f32; 4]) -> impl FnOnce(&mut WorkspaceSlot) {
    move |slot| {
        slot.name = name;
        slot.accent_color = accent;
    }
}

/// Requested pinned-board height plus the terminal space that must remain
/// below it in the same region.
#[derive(Clone, Copy)]
pub struct BoardReservation {
    pub height: f32,
    pub terminal: f32,
}

/// The window's live pane/workspace layout.
pub struct PaneShell {
    /// Window-level split into workspace regions.
    workspace: Entity<WorkspaceTree>,
    /// One pane split tree per workspace region.
    trees: HashMap<WorkspaceId, Entity<PaneTree>>,
    /// The focused pane inside each region, so moving between regions restores
    /// the pane the user was last in rather than jumping to the root.
    focused: HashMap<WorkspaceId, PaneId>,
    /// Which session each live pane is showing.
    sessions: HashMap<PaneId, SessionId>,
    /// Panes waiting for a session. A split (or a new tab) asks the server to
    /// create one and the answer arrives asynchronously on the reader thread,
    /// so the pane that asked is queued here and claims the next session the
    /// reconcile pass finds unattached.
    pending: VecDeque<PaneId>,
    /// Whether the root region has adopted the server's real workspace ID.
    adopted_server_workspace: bool,
    /// Regions that asked the server for a workspace and are still showing a
    /// client-minted id, oldest first. `CreateWorkspace` carries no id, so the
    /// answering `WorkspaceInfo` is matched to the region that asked by the
    /// FIFO order of the one ordered writer channel.
    pending_workspaces: VecDeque<WorkspaceId>,
    /// Regions whose id the server itself minted. Only these may be named in a
    /// `CloseWorkspace` or a `MoveSession`: the server has never heard of a
    /// client-minted id and would reject or misapply one.
    server_workspaces: HashSet<WorkspaceId>,
    /// Regions giving up a top strip to a pinned Beads board, and how much.
    pinned_boards: HashMap<WorkspaceId, f32>,
    /// Regions giving up a collapsed or expanded CI strip below their tab chrome.
    ci_strips: HashMap<WorkspaceId, f32>,
}

impl PaneShell {
    /// Build a single-region, single-pane shell.
    ///
    /// The region starts on a client-minted [`WorkspaceId`] because the shell
    /// exists before the first `Welcome`; [`Self::adopt_server_workspace`]
    /// renames it once the server names one.
    pub fn new(accent: [f32; 4], cx: &mut App) -> Self {
        let workspace_id = WorkspaceId::new();
        let workspace = cx.new(|_| WorkspaceTree::new(workspace_id, Some(accent)));
        let tree = cx.new(|_| PaneTree::new());
        let root = tree.read(cx).initial_pane_id();
        Self {
            workspace,
            trees: HashMap::from([(workspace_id, tree)]),
            focused: HashMap::from([(workspace_id, root)]),
            sessions: HashMap::new(),
            pending: VecDeque::new(),
            adopted_server_workspace: false,
            pending_workspaces: VecDeque::new(),
            server_workspaces: HashSet::new(),
            pinned_boards: HashMap::new(),
            ci_strips: HashMap::new(),
        }
    }

    /// Rename the root region to the workspace the server actually owns.
    ///
    /// Runs once, off the first `SessionList`: the shell exists before the
    /// server has named anything, so the root region starts client-local and is
    /// re-keyed here. Every *later* region instead asks for its own workspace
    /// with `ClientMessage::CreateWorkspace` and adopts the answer through
    /// [`Self::apply_workspace_info`].
    pub fn adopt_server_workspace(&mut self, server_id: WorkspaceId, cx: &mut App) -> bool {
        if self.adopted_server_workspace {
            return false;
        }
        let current = self.focused_workspace_id(cx);
        if current == server_id {
            self.adopted_server_workspace = true;
            self.server_workspaces.insert(server_id);
            return false;
        }
        if !self.rename_region(current, server_id, cx) {
            return false;
        }
        self.adopted_server_workspace = true;
        true
    }

    /// Queue `workspace_id` as a region waiting for the server to mint one.
    ///
    /// Called right after the `CreateWorkspace` the split sent, so the answering
    /// `WorkspaceInfo` lands on the region that asked for it.
    fn expect_server_workspace(&mut self, workspace_id: WorkspaceId) {
        self.pending_workspaces.push_back(workspace_id);
    }

    /// Fold one `WorkspaceInfo` onto the region it describes.
    ///
    /// The id is either already in the layout — a routine metadata refresh for
    /// a region the window shows — or it is the answer to a `CreateWorkspace`,
    /// in which case the oldest region still waiting for a server workspace
    /// adopts it. Anything else names a workspace this window does not show and
    /// is reported as [`WorkspaceInfoOutcome::Unclaimed`] rather than silently
    /// re-keying an unrelated region.
    pub fn apply_workspace_info(
        &mut self,
        info: &WorkspaceInfo,
        cx: &mut App,
    ) -> WorkspaceInfoOutcome {
        let known = self.workspace.read(cx).find_workspace(info.workspace_id).is_some();
        let outcome = if known {
            WorkspaceInfoOutcome::Updated
        } else if self.adopt_pending_workspace(info.workspace_id, cx) {
            WorkspaceInfoOutcome::Adopted
        } else {
            return WorkspaceInfoOutcome::Unclaimed;
        };
        self.server_workspaces.insert(info.workspace_id);
        let name = info.name.clone();
        let project_root = info.project_root.clone();
        // A malformed accent leaves the region's current tint alone rather than
        // blanking the focus ring.
        let accent = info.accent.or_else(|| {
            self.workspace.read(cx).find_workspace(info.workspace_id).map(|slot| slot.accent_color)
        });
        let edit = |slot: &mut WorkspaceSlot| {
            slot.name = name;
            slot.project_root = project_root;
            slot.accent_color = accent.unwrap_or(slot.accent_color);
        };
        self.workspace.update(cx, |tree, ctx| tree.update_slot(info.workspace_id, edit, ctx));
        outcome
    }

    /// Re-key the oldest region still waiting for a server workspace onto
    /// `server_id`. Regions that were closed while their answer was in flight
    /// are discarded rather than re-created.
    fn adopt_pending_workspace(&mut self, server_id: WorkspaceId, cx: &mut App) -> bool {
        while let Some(local_id) = self.pending_workspaces.pop_front() {
            if self.trees.contains_key(&local_id) && self.rename_region(local_id, server_id, cx) {
                return true;
            }
        }
        false
    }

    /// Move a region — its slot, its pane tree, and its focused pane — from
    /// `old_id` onto `new_id`, keeping the server-known set in step.
    fn rename_region(&mut self, old_id: WorkspaceId, new_id: WorkspaceId, cx: &mut App) -> bool {
        if !self.workspace.update(cx, |tree, ctx| tree.set_workspace_id(old_id, new_id, ctx)) {
            return false;
        }
        if let Some(tree) = self.trees.remove(&old_id) {
            self.trees.insert(new_id, tree);
        }
        if let Some(pane) = self.focused.remove(&old_id) {
            self.focused.insert(new_id, pane);
        }
        self.server_workspaces.remove(&old_id);
        self.server_workspaces.insert(new_id);
        true
    }

    /// Whether the server itself minted `workspace_id`.
    pub fn is_server_workspace(&self, workspace_id: WorkspaceId) -> bool {
        self.server_workspaces.contains(&workspace_id)
    }

    /// Whether this window shows a region for `workspace_id`.
    pub fn has_region(&self, workspace_id: WorkspaceId) -> bool {
        self.trees.contains_key(&workspace_id)
    }

    /// The region containing `pane`, or `None` once the pane has been closed.
    pub fn region_for_pane(&self, pane: PaneId, cx: &App) -> Option<WorkspaceId> {
        self.region_of(pane, cx)
    }

    /// Serialize the window's live layout in the frozen `ReportWorkspaceTree`
    /// shape.
    ///
    /// The workspace topology comes from the [`WorkspaceTree`] that already
    /// owns it; the per-region payload is filled in from this shell's own pane
    /// trees and from `tabs`, the window's ordered strip, because the GPUI shell
    /// keeps panes in [`PaneTree`] entities and its tabs in a window-wide strip
    /// rather than in the workspace model's per-region tab list.
    ///
    /// `tabs` is what makes the report *complete*. A region's leaf carries the
    /// whole ordered tab list, not just the sessions currently in panes: the
    /// server persists this tree per window and hands it back on the next
    /// connect, so anything left out of it is state the user loses on restart.
    /// Reporting only the visible pane — which this did — left the server with a
    /// one-element order and `active_tab_index: 0`, so tab order and the active
    /// tab could not survive a reconnect no matter what the rest of the restore
    /// path did.
    ///
    /// Panes that are still waiting for a session are pruned instead of being
    /// serialized under a synthetic id: a reconnect must not restore a pane
    /// pointing at a session that never existed.
    pub fn wire_tree(&self, tabs: &TabSessions, cx: &App) -> WorkspaceTreeNode {
        let topology = self.workspace.read(cx).to_tree();
        self.fill_regions(topology, tabs, cx)
    }

    /// Replace each leaf's (empty) tab payload with the region's live tabs.
    fn fill_regions(
        &self,
        node: WorkspaceTreeNode,
        tabs: &TabSessions,
        cx: &App,
    ) -> WorkspaceTreeNode {
        match node {
            WorkspaceTreeNode::Leaf { workspace_id, .. } => {
                let displayed = self.trees.get(&workspace_id).and_then(|tree| {
                    pane_node_to_wire(tree.read(cx).tree().root(), &self.sessions)
                });
                let active = self
                    .focused
                    .get(&workspace_id)
                    .and_then(|pane| self.sessions.get(pane))
                    .copied();
                let region = region_tab_payload(workspace_id, tabs, displayed, active);
                WorkspaceTreeNode::Leaf {
                    workspace_id,
                    session_ids: region.session_ids,
                    pane_trees: region.pane_trees,
                    active_tab_index: region.active_tab_index,
                }
            }
            WorkspaceTreeNode::Split { direction, ratio, first, second } => {
                WorkspaceTreeNode::Split {
                    direction,
                    ratio,
                    first: Box::new(self.fill_regions(*first, tabs, cx)),
                    second: Box::new(self.fill_regions(*second, tabs, cx)),
                }
            }
        }
    }

    /// The focused workspace region's ID.
    pub fn focused_workspace_id(&self, cx: &App) -> WorkspaceId {
        self.workspace.read(cx).focused_workspace_id()
    }

    /// The server-derived project root for the focused workspace region.
    ///
    /// `Some` only while the region's CWD sits under a configured
    /// `workspaces.roots` entry — the server clears it as soon as the pane
    /// leaves, so this doubles as "is the focus currently in a workspace".
    pub fn focused_workspace_project_root(&self, cx: &App) -> Option<std::path::PathBuf> {
        let workspace = self.workspace.read(cx);
        workspace
            .find_workspace(workspace.focused_workspace_id())
            .and_then(|slot| slot.project_root.clone())
    }

    /// Every visible region whose server-derived project root is known.
    pub fn region_project_roots(&self, cx: &App) -> Vec<(WorkspaceId, std::path::PathBuf)> {
        let workspace = self.workspace.read(cx);
        workspace
            .layout()
            .compute_workspace_rects(Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 })
            .into_iter()
            .filter_map(|(workspace_id, _)| {
                workspace
                    .find_workspace(workspace_id)
                    .and_then(|slot| slot.project_root.clone())
                    .map(|root| (workspace_id, root))
            })
            .collect()
    }

    /// Each region's left edge keyed by workspace, for aligning the strip's
    /// workspace tab groups over their regions.
    pub fn region_left_edges(&self, viewport: Rect, cx: &App) -> HashMap<WorkspaceId, f32> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .map(|(workspace_id, rect)| (workspace_id, rect.x))
            .collect()
    }

    /// Whether `rect` hangs below the window's top row of regions, and so
    /// carries its own tab bar instead of a titlebar group.
    fn is_lower_region(rect: Rect) -> bool {
        rect.y > 0.5
    }

    /// `rect` minus everything a region reserves at its top: lower-region tabs,
    /// the collapsed CI band, then a pinned Beads board.
    ///
    /// Both reservations land here rather than at the call sites so pane math,
    /// painted panes, dividers, and the rows published to the PTY cannot
    /// disagree about where a region's content starts.
    fn content_rect(rect: Rect, ci: f32, board: f32) -> Rect {
        let bar = if Self::is_lower_region(rect) { REGION_TAB_BAR_HEIGHT } else { 0.0 };
        let reserved = (bar + ci + board).min(rect.height);
        Rect { x: rect.x, y: rect.y + reserved, width: rect.width, height: rect.height - reserved }
    }

    /// The collapsed CI strip inside one raw region rect, directly below tabs.
    fn ci_rect(rect: Rect, strip: f32) -> Option<Rect> {
        let tab = if Self::is_lower_region(rect) { REGION_TAB_BAR_HEIGHT } else { 0.0 };
        let height = strip.min((rect.height - tab).max(0.0));
        (height > 0.0).then_some(Rect { x: rect.x, y: rect.y + tab, width: rect.width, height })
    }

    fn ci_strip(&self, workspace_id: WorkspaceId) -> f32 {
        self.ci_strips.get(&workspace_id).copied().unwrap_or_default()
    }

    /// Publish each visible CI region's collapsed or expanded reservation.
    pub fn set_ci_strips(&mut self, strips: HashMap<WorkspaceId, f32>) {
        self.ci_strips = strips;
    }

    /// Resolve one visible collapsed CI band against the region layout.
    pub fn ci_bar_rect(&self, workspace_id: WorkspaceId, viewport: Rect, cx: &App) -> Option<Rect> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .and_then(|(_, rect)| Self::ci_rect(rect, self.ci_strip(workspace_id)))
    }

    /// The strip a pinned board reserves inside `workspace_id`'s region, which
    /// is zero for every region whose board is not pinned.
    fn board_strip(&self, workspace_id: WorkspaceId) -> f32 {
        self.pinned_boards.get(&workspace_id).copied().unwrap_or(0.0)
    }

    /// Record which regions give up a strip to a pinned Beads board. Every
    /// region pins independently, so this is a map and not one entry.
    pub fn set_pinned_boards(&mut self, pinned: HashMap<WorkspaceId, f32>) {
        self.pinned_boards = pinned;
    }

    /// The strip a board of `height` paints in at the top of `workspace_id`'s
    /// region: the region's own content area, never the window's.
    pub fn board_rect(
        &self,
        workspace_id: WorkspaceId,
        height: f32,
        viewport: Rect,
        cx: &App,
    ) -> Option<Rect> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .map(|(_, rect)| Self::content_rect(rect, self.ci_strip(workspace_id), 0.0))
            .map(|content| Rect { height: height.min(content.height), ..content })
    }

    /// A pinned board rect capped before `terminal_reservation`, so stored
    /// height remains a preference while a short stacked region still keeps
    /// its terminal recoverable.
    pub fn reserved_board_rect(
        &self,
        workspace_id: WorkspaceId,
        reservation: BoardReservation,
        viewport: Rect,
        cx: &App,
    ) -> Option<Rect> {
        let content = self.board_rect(workspace_id, f32::MAX, viewport, cx)?;
        Some(Rect {
            height: Self::reserved_board_height(
                content.height,
                reservation.height,
                reservation.terminal,
            ),
            ..content
        })
    }

    fn reserved_board_height(content: f32, requested: f32, terminal_reservation: f32) -> f32 {
        let reservation =
            if terminal_reservation.is_finite() { terminal_reservation.max(0.0) } else { 0.0 };
        requested.min((content - reservation).max(0.0))
    }

    /// Full region bounds used to clamp workspace-owned overlays.
    pub fn workspace_rect(
        &self,
        workspace_id: WorkspaceId,
        viewport: Rect,
        cx: &App,
    ) -> Option<Rect> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .map(|(_, rect)| rect)
    }

    /// The tab-bar strip each lower region reserves at its top, in region
    /// left-to-right, top-to-bottom order.
    pub fn region_bar_rects(&self, viewport: Rect, cx: &App) -> Vec<(WorkspaceId, Rect)> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .filter(|(_, rect)| Self::is_lower_region(*rect))
            .map(|(workspace_id, rect)| {
                let bar = REGION_TAB_BAR_HEIGHT.min(rect.height);
                (workspace_id, Rect { x: rect.x, y: rect.y, width: rect.width, height: bar })
            })
            .collect()
    }

    /// The session `workspace_id`'s region currently shows: its focused pane's
    /// session. This is the tab an in-region bar highlights, independent of
    /// which region owns the window's focus.
    pub fn region_shown_session(&self, workspace_id: WorkspaceId) -> Option<SessionId> {
        self.focused.get(&workspace_id).and_then(|pane| self.sessions.get(pane)).copied()
    }

    /// `workspace_id`'s region accent, for the strip's per-workspace badges.
    pub fn workspace_accent(&self, workspace_id: WorkspaceId, cx: &App) -> [f32; 4] {
        self.workspace
            .read(cx)
            .find_workspace(workspace_id)
            .map_or(FALLBACK_PANE_ACCENT, |slot| slot.accent_color)
    }

    /// The focused pane of the focused region.
    pub fn focused_pane(&self, cx: &App) -> Option<PaneId> {
        self.focused.get(&self.focused_workspace_id(cx)).copied()
    }

    /// The focused pane of `workspace_id`'s region, independent of which region
    /// owns the window's focus. This is the pane one of that region's tabs
    /// belongs in.
    pub fn region_focused_pane(&self, workspace_id: WorkspaceId) -> Option<PaneId> {
        self.focused.get(&workspace_id).copied()
    }

    /// The session the focused pane is showing.
    pub fn focused_session(&self, cx: &App) -> Option<SessionId> {
        self.focused_pane(cx).and_then(|pane| self.sessions.get(&pane).copied())
    }

    /// Every session currently shown in a pane.
    pub fn shown_sessions(&self) -> HashSet<SessionId> {
        self.sessions.values().copied().collect()
    }

    /// The `(region, pane)` showing `session_id`, if any.
    pub fn pane_for_session(
        &self,
        session_id: SessionId,
        cx: &App,
    ) -> Option<(WorkspaceId, PaneId)> {
        let pane =
            self.sessions.iter().find_map(|(pane, sid)| (*sid == session_id).then_some(*pane))?;
        let workspace = self.region_of(pane, cx)?;
        Some((workspace, pane))
    }

    /// Point `pane` at `session_id`, returning the session it displaced.
    pub fn assign_session(&mut self, pane: PaneId, session_id: SessionId) -> Option<SessionId> {
        self.sessions.insert(pane, session_id).filter(|prev| *prev != session_id)
    }

    /// Focus `pane` inside `workspace_id`, making that region focused too.
    pub fn focus_pane(&mut self, workspace_id: WorkspaceId, pane: PaneId, cx: &mut App) {
        self.focused.insert(workspace_id, pane);
        if self.focused_workspace_id(cx) != workspace_id {
            self.workspace.update(cx, |tree, ctx| tree.set_focused_workspace(workspace_id, ctx));
        }
    }

    /// Whether any pane is still waiting for the session it asked for.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Take the pane at the head of the pending queue, dropping panes that have
    /// been closed while their session was in flight.
    pub fn take_pending(&mut self, cx: &App) -> Option<PaneId> {
        while let Some(pane) = self.pending.pop_front() {
            if self.region_of(pane, cx).is_some() {
                return Some(pane);
            }
        }
        None
    }

    /// Split the focused pane, moving focus onto the new one and queueing it for
    /// the session the caller is about to request.
    pub fn split_focused_pane(
        &mut self,
        direction: SplitDirection,
        cx: &mut App,
    ) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let focused = self.focused.get(&workspace_id).copied()?;
        let tree = self.trees.get(&workspace_id)?.clone();
        let new_pane =
            tree.update(cx, |pane_tree, ctx| pane_tree.split(focused, direction, ctx))?;
        self.focused.insert(workspace_id, new_pane);
        self.pending.push_back(new_pane);
        Some(new_pane)
    }

    /// Close the focused pane, or the whole region when it was the region's last
    /// pane and other regions remain.
    pub fn close_focused_pane(&mut self, cx: &mut App) -> ClosedPane {
        let Some(focused) = self.focused_pane(cx) else {
            return ClosedPane::LastPane;
        };
        let Retired::Pane(closed_region) = self.retire_pane(focused, cx) else {
            return ClosedPane::LastPane;
        };
        ClosedPane::Removed {
            sessions: self.sessions.remove(&focused).into_iter().collect(),
            closed_region,
        }
    }

    /// Remove `pane` from the layout, collapsing its region when it was the
    /// region's last pane and other regions remain.
    ///
    /// Returns [`Retired::Nothing`] when nothing could be removed — the window
    /// is down to one pane in one region — which is the case both callers
    /// translate into "close the tab instead".
    fn retire_pane(&mut self, pane: PaneId, cx: &mut App) -> Retired {
        let Some(workspace_id) = self.region_of(pane, cx) else { return Retired::Nothing };
        let Some(pane_tree) = self.trees.get(&workspace_id).cloned() else {
            return Retired::Nothing;
        };
        if pane_tree.read(cx).all_pane_ids().len() > 1 {
            let next = pane_tree.read(cx).next_pane(pane);
            pane_tree.update(cx, |panes, ctx| panes.close(pane, ctx));
            self.refocus_after_close(workspace_id, pane, next);
            return Retired::Pane(None);
        }
        if self.workspace.read(cx).layout().workspace_count() <= 1 {
            return Retired::Nothing;
        }
        self.trees.remove(&workspace_id);
        self.focused.remove(&workspace_id);
        self.workspace.update(cx, |window, ctx| window.remove_workspace(workspace_id, ctx));
        self.pending_workspaces.retain(|id| *id != workspace_id);
        // Only a region the server minted can be closed on the server; one that
        // is still waiting for its `WorkspaceInfo` names an id the server has
        // never seen.
        Retired::Pane(self.server_workspaces.take(&workspace_id))
    }

    /// Move a region's focus off a pane that has just been closed.
    fn refocus_after_close(&mut self, workspace_id: WorkspaceId, closed: PaneId, next: PaneId) {
        if self.focused.get(&workspace_id) == Some(&closed) {
            self.focused.insert(workspace_id, next);
        }
    }

    /// Cycle focus to the next pane of the focused region in depth-first order.
    pub fn focus_next_pane(&mut self, cx: &mut App) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let focused = self.focused.get(&workspace_id).copied()?;
        let tree = self.trees.get(&workspace_id)?;
        let next = tree.read(cx).next_pane(focused);
        if next == focused {
            return None;
        }
        self.focused.insert(workspace_id, next);
        Some(next)
    }

    /// Move pane focus spatially inside the focused region.
    pub fn focus_pane_in_direction(
        &mut self,
        direction: FocusDirection,
        viewport: Rect,
        cx: &mut App,
    ) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let focused = self.focused.get(&workspace_id).copied()?;
        let region = self.region_rect(workspace_id, viewport, cx)?;
        let tree = self.trees.get(&workspace_id)?;
        let pane_tree = tree.read(cx);
        let rects = pane_tree.compute_rects(region);
        let next = pane_tree.find_pane_in_direction(focused, direction, &rects)?;
        if next == focused {
            return None;
        }
        self.focused.insert(workspace_id, next);
        Some(next)
    }

    /// Split the window into a new workspace region, focus it, and queue its
    /// root pane for the session the caller is about to request.
    ///
    /// The region is minted client-local and immediately queued as awaiting a
    /// server workspace, so the caller's `CreateWorkspace` and the answering
    /// `WorkspaceInfo` re-key it through [`Self::apply_workspace_info`]. The
    /// accent passed here is therefore a placeholder the server's own palette
    /// colour replaces one round trip later.
    pub fn split_workspace(
        &mut self,
        direction: SplitDirection,
        accent: [f32; 4],
        cx: &mut App,
    ) -> Option<WorkspaceId> {
        let new_id = self
            .workspace
            .update(cx, |tree, ctx| tree.split_workspace(direction, Some(accent), ctx))?;
        let tree = cx.new(|_| PaneTree::new());
        let root = tree.read(cx).initial_pane_id();
        self.trees.insert(new_id, tree);
        self.focused.insert(new_id, root);
        self.pending.push_back(root);
        self.expect_server_workspace(new_id);
        Some(new_id)
    }

    /// Move focus to the neighbouring workspace region.
    pub fn focus_workspace_in_direction(
        &mut self,
        direction: FocusDirection,
        viewport: Rect,
        cx: &mut App,
    ) -> Option<WorkspaceId> {
        let current = self.focused_workspace_id(cx);
        let target =
            self.workspace.read(cx).layout().find_workspace_in_direction(direction, viewport)?;
        if target == current {
            return None;
        }
        self.workspace.update(cx, |tree, ctx| tree.set_focused_workspace(target, ctx));
        Some(target)
    }

    /// Drop panes whose session has exited, collapsing regions that empty out.
    ///
    /// Reports both whether the layout changed — so the caller can republish
    /// pane sizes and repaint — and which server-minted regions collapsed, so
    /// the caller can close them on the server too.
    ///
    /// `workspaces_with_tabs` names the workspaces the strip still holds tabs
    /// for. A region whose last pane dies while its workspace still has tabs
    /// keeps that pane empty instead of collapsing — the reconcile refill
    /// hands it the workspace's refocused tab — so a hidden tab can never be
    /// orphaned by its shown sibling's exit.
    pub fn retain_sessions(
        &mut self,
        live: &HashSet<SessionId>,
        workspaces_with_tabs: &HashSet<WorkspaceId>,
        cx: &mut App,
    ) -> RetiredPanes {
        let dead: Vec<PaneId> = self
            .sessions
            .iter()
            .filter_map(|(pane, session)| (!live.contains(session)).then_some(*pane))
            .collect();
        let mut retired = RetiredPanes::default();
        for pane in dead {
            self.sessions.remove(&pane);
            let survives = self.region_of(pane, cx).is_some_and(|workspace_id| {
                workspaces_with_tabs.contains(&workspace_id)
                    && self
                        .trees
                        .get(&workspace_id)
                        .is_some_and(|tree| tree.read(cx).all_pane_ids().len() == 1)
            });
            if survives {
                retired.changed = true;
                continue;
            }
            // A sole pane in a sole region survives its session: the window
            // keeps its empty pane and the next attach fills it back in.
            if let Retired::Pane(Some(region)) = self.retire_pane(pane, cx) {
                retired.closed_regions.push(region);
            }
            retired.changed = true;
        }
        retired
    }

    /// Every pane with no session that is not already queued for one — the
    /// panes the reconcile refill may hand an unshown tab of their region's
    /// workspace.
    pub fn empty_unpending_panes(&self, cx: &App) -> Vec<(WorkspaceId, PaneId)> {
        self.trees
            .iter()
            .flat_map(|(workspace_id, tree)| {
                tree.read(cx)
                    .all_pane_ids()
                    .into_iter()
                    .filter(|pane| {
                        !self.sessions.contains_key(pane) && !self.pending.contains(pane)
                    })
                    .map(|pane| (*workspace_id, pane))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Resolve every pane against `viewport` for one frame.
    pub fn placements(&self, viewport: Rect, cx: &App) -> Vec<PanePlacement> {
        let workspace = self.workspace.read(cx);
        let focused_workspace = workspace.focused_workspace_id();
        let mut out = Vec::new();
        for (workspace_id, region) in workspace.layout().compute_workspace_rects(viewport) {
            let region = Self::content_rect(
                region,
                self.ci_strip(workspace_id),
                self.board_strip(workspace_id),
            );
            let Some(tree) = self.trees.get(&workspace_id) else { continue };
            let accent = workspace
                .find_workspace(workspace_id)
                .map_or(FALLBACK_PANE_ACCENT, |slot: &WorkspaceSlot| slot.accent_color);
            let focused_pane = self.focused.get(&workspace_id).copied();
            for (pane_id, rect, _edges) in tree.read(cx).compute_rects(region) {
                out.push(PanePlacement {
                    workspace_id,
                    pane_id,
                    session_id: self.sessions.get(&pane_id).copied(),
                    rect,
                    focused: workspace_id == focused_workspace && focused_pane == Some(pane_id),
                    accent,
                });
            }
        }
        out
    }

    /// Resolve every live pane divider against the grid viewport.
    ///
    /// The pane trees own their ratios while this shell owns their regions, so
    /// the running view gets both pieces here rather than reimplementing the
    /// tree traversal beside its paint code.
    pub fn dividers(&self, viewport: Rect, cx: &App) -> Vec<Divider> {
        let workspace = self.workspace.read(cx);
        workspace
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .flat_map(|(workspace_id, region)| {
                let region = Self::content_rect(
                    region,
                    self.ci_strip(workspace_id),
                    self.board_strip(workspace_id),
                );
                self.trees.get(&workspace_id).into_iter().flat_map(move |tree| {
                    divider::collect_dividers(tree.read(cx).tree().root(), region)
                })
            })
            .collect()
    }

    /// Resolve every workspace-region divider against the grid viewport.
    pub fn workspace_dividers(&self, viewport: Rect, cx: &App) -> Vec<WorkspaceDivider> {
        self.workspace.read(cx).layout().collect_workspace_dividers(viewport)
    }

    /// Set the ratio of the split between two workspace regions.
    pub fn set_workspace_ratio(
        &mut self,
        first_workspace: WorkspaceId,
        second_workspace: WorkspaceId,
        ratio: f32,
        cx: &mut App,
    ) -> bool {
        self.workspace.update(cx, |tree, ctx| {
            tree.set_workspace_ratio(first_workspace, second_workspace, ratio, ctx)
        })
    }

    /// Set the split ratio containing `pane_id` and report whether it changed.
    pub fn set_pane_ratio(&mut self, pane_id: PaneId, ratio: f32, cx: &mut App) -> bool {
        let Some(workspace_id) = self.region_of(pane_id, cx) else { return false };
        let Some(tree) = self.trees.get(&workspace_id).cloned() else { return false };
        tree.update(cx, |pane_tree, ctx| pane_tree.set_ratio(pane_id, ratio, ctx))
    }

    /// Reset every workspace-region and pane split so all surfaces share the
    /// window evenly — the balance affordance behind the status-bar button
    /// and the titlebar equalize icon.
    pub fn equalize_all(&mut self, cx: &mut App) {
        self.workspace.update(cx, WorkspaceTree::equalize_ratios);
        for tree in self.trees.values() {
            tree.update(cx, PaneTree::equalize);
        }
    }

    /// Total number of live panes across every region.
    pub fn pane_count(&self, cx: &App) -> usize {
        self.trees.values().map(|tree| tree.read(cx).all_pane_ids().len()).sum()
    }

    /// Number of panes in the focused region's active tab.
    pub fn focused_region_pane_count(&self, cx: &App) -> usize {
        self.trees
            .get(&self.focused_workspace_id(cx))
            .map_or(0, |tree| tree.read(cx).all_pane_ids().len())
    }

    /// Number of workspace regions the window is split into.
    pub fn region_count(&self, cx: &App) -> usize {
        self.workspace.read(cx).layout().workspace_count()
    }

    /// Every workspace this window currently shows a region for.
    pub fn region_workspaces(&self, cx: &App) -> HashSet<WorkspaceId> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(Rect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 })
            .into_iter()
            .map(|(workspace_id, _)| workspace_id)
            .collect()
    }

    // -- Cold-restart restore -------------------------------------------------

    /// Serialise this window's live regions and panes into a cold-restart
    /// snapshot.
    ///
    /// The shell keeps panes in [`PaneTree`] entities rather than in the
    /// workspace model's tab list, while the ported snapshot format is written
    /// against [`WindowLayout`] — so a scratch layout is filled from the live
    /// trees (one region → one tab whose pane tree is the region's whole split,
    /// the same shape [`Self::wire_tree`] reports to the server) and handed to
    /// [`snapshot_window_restore`]. Panes still waiting for a session are
    /// pruned, exactly as they are on the wire: replaying a pane whose session
    /// never existed would recreate a launch the user never made.
    ///
    /// `prompts` is the live prompt-bar history keyed by session. It is passed
    /// in rather than read from the shell because the GPUI client keeps it in
    /// the IPC-written `AiChrome` map instead of on the pane, and a snapshot
    /// that cannot see it persists `prompt_count: 0` for every AI pane — which
    /// is enough to make the restored bar render as absent.
    pub fn restore_snapshot(
        &self,
        window_id: WindowId,
        bindings: &HashMap<SessionId, LaunchBinding>,
        prompts: &HashMap<SessionId, PromptBarData>,
        cx: &App,
    ) -> WindowRestoreState {
        let workspace = self.workspace.read(cx);
        let mut layout = WindowLayout::from_tree(&workspace.to_tree());
        let mut panes: HashMap<PaneId, PaneRestore> = HashMap::new();
        for workspace_id in workspace.layout().workspace_ids_in_order() {
            if let (Some(live), Some(target)) =
                (workspace.find_workspace(workspace_id), layout.find_workspace_mut(workspace_id))
            {
                target.name.clone_from(&live.name);
                target.accent_color = live.accent_color;
            }
            self.snapshot_region(
                workspace_id,
                &mut SnapshotTarget { layout: &mut layout, panes: &mut panes },
                &SnapshotSources { bindings, prompts },
                cx,
            );
        }
        layout.set_focused_workspace(workspace.focused_workspace_id());
        snapshot_window_restore(window_id, &layout, &panes)
    }

    /// Copy one region's pane split into the scratch layout as a single tab and
    /// record a [`PaneRestore`] for every pane that has a session.
    fn snapshot_region(
        &self,
        workspace_id: WorkspaceId,
        target: &mut SnapshotTarget<'_>,
        sources: &SnapshotSources<'_>,
        cx: &App,
    ) {
        let SnapshotTarget { layout, panes } = target;
        let Some(tree) = self.trees.get(&workspace_id) else { return };
        let Some(wire) = pane_node_to_wire(tree.read(cx).tree().root(), &self.sessions) else {
            return;
        };
        let Some(pairs) = layout.add_tab_with_pane_tree(workspace_id, first_session(&wire), &wire)
        else {
            return;
        };
        let focused_session =
            self.focused.get(&workspace_id).and_then(|pane| self.sessions.get(pane)).copied();
        let mut focused_pane = None;
        for (session_id, pane_id) in pairs {
            if Some(session_id) == focused_session {
                focused_pane = Some(pane_id);
            }
            let launch_binding = sources
                .bindings
                .get(&session_id)
                .cloned()
                .unwrap_or_else(|| new_shell_binding(None));
            panes.insert(
                pane_id,
                PaneRestore {
                    session_id,
                    workspace_id,
                    cwd: launch_binding.fallback_cwd.clone(),
                    launch_binding,
                    prompts: sources.prompts.get(&session_id).cloned().unwrap_or_default(),
                    last_conversation_id: None,
                },
            );
        }
        if let (Some(pane), Some(tab)) =
            (focused_pane, layout.active_tab_for_workspace_mut(workspace_id))
        {
            tab.focused_pane = pane;
        }
    }

    /// Replace the whole shell with a window rebuilt from a cold-restart
    /// snapshot, returning the ordered launch queue in pane order.
    ///
    /// Every restored pane is queued as pending, so the sessions the caller is
    /// about to ask for land back in the panes they came from as their
    /// `SessionCreated` answers arrive. The restored regions are marked as
    /// server workspaces because the replay names them in its own
    /// `CreateSession` frames, which is what registers them with the server's
    /// workspace manager.
    pub fn adopt_restored(
        &mut self,
        mut rebuilt: RebuiltWindow,
        cx: &mut App,
    ) -> Vec<ReplayLaunch> {
        let topology = rebuilt.layout.to_tree(&HashMap::new());
        let focused_workspace = rebuilt.layout.focused_workspace_id();
        let region_ids = rebuilt.layout.workspace_ids_in_order();
        let mut trees = HashMap::new();
        let mut focused = HashMap::new();
        let mut slots = Vec::new();
        for workspace_id in &region_ids {
            let Some(slot) = rebuilt.layout.find_workspace_mut(*workspace_id) else { continue };
            slots.push((*workspace_id, slot.name.clone(), slot.accent_color));
            let active = slot.active_tab;
            let restored = slot.tabs.get_mut(active).map(|tab| {
                (std::mem::replace(&mut tab.pane_layout, LayoutTree::new()), tab.focused_pane)
            });
            let (pane_layout, wanted_focus) = match restored {
                Some((pane_layout, pane)) => (pane_layout, Some(pane)),
                None => (LayoutTree::new(), None),
            };
            let tree = cx.new(|_| PaneTree::from_tree(pane_layout));
            let focused_pane = wanted_focus
                .filter(|pane| tree.read(cx).all_pane_ids().contains(pane))
                .unwrap_or_else(|| tree.read(cx).initial_pane_id());
            focused.insert(*workspace_id, focused_pane);
            trees.insert(*workspace_id, tree);
        }

        self.workspace = cx.new(|_| WorkspaceTree::from_tree(&topology));
        self.workspace.update(cx, |tree, ctx| {
            for (workspace_id, name, accent) in slots {
                tree.update_slot(workspace_id, apply_slot_metadata(name, accent), ctx);
            }
            tree.set_focused_workspace(focused_workspace, ctx);
        });
        self.trees = trees;
        self.focused = focused;
        self.sessions.clear();
        self.pending = rebuilt.launches.iter().map(|launch| launch.pane_id).collect();
        // The snapshot's regions are the ids the replay's own `CreateSession`
        // frames name, so the server knows them from the first launch onward and
        // nothing here is waiting on a `WorkspaceInfo` to be re-keyed.
        self.adopted_server_workspace = true;
        self.pending_workspaces.clear();
        self.server_workspaces = region_ids.into_iter().collect();
        rebuilt.launches.into_iter().collect()
    }

    /// Whether the shell still shows the untouched startup layout: one region,
    /// one pane, no session shown, and nothing in flight.
    ///
    /// Only such a shell may adopt the server's persisted workspace tree
    /// wholesale on reconnect — anything else is a layout the user (or a
    /// cold-restart replay) already owns, and the live layout must win.
    pub fn is_unused(&self, cx: &App) -> bool {
        self.sessions.is_empty()
            && self.pending.is_empty()
            && self.region_count(cx) == 1
            && self.pane_count(cx) <= 1
    }

    /// Replace the whole shell with the workspace tree the server persisted
    /// for this window, returning the sessions now visible in panes (layout
    /// order) so the caller can attach each one.
    ///
    /// The hot-reconnect twin of [`Self::adopt_restored`]: the sessions
    /// already exist on the server, so the tree's own session ids are placed
    /// directly instead of queueing placeholder launches. Sessions named in
    /// the tree but absent from `live` are pruned the way the cold path prunes
    /// panes without sessions; a tree that prunes away entirely leaves the
    /// shell untouched. Each region shows its displayed tab's pane split — the
    /// GPUI shell keeps one pane tree per region — and any other tab sessions
    /// stay reachable through the flat tab strip.
    pub fn adopt_server_tree(
        &mut self,
        tree: &WorkspaceTreeNode,
        live: &HashSet<SessionId>,
        cx: &mut App,
    ) -> Vec<SessionId> {
        let Some(pruned) = prune_workspace_node(tree, live) else { return Vec::new() };
        let mut leaves = Vec::new();
        wire_leaf_display_tabs(&pruned, &mut leaves);
        let mut trees = HashMap::new();
        let mut focused = HashMap::new();
        let mut sessions = HashMap::new();
        let mut visible = Vec::new();
        for (workspace_id, displayed, active) in &leaves {
            let (root, pairs) = pane_tree_to_layout_node(displayed);
            let Some(&(_, first_pane)) = pairs.first() else { continue };
            // Focus the pane showing the tab this region reported as active, so
            // a restored split comes back typed-into where the user left it
            // rather than always in its leftmost pane.
            let active_pane = active
                .and_then(|session_id| {
                    pairs.iter().find(|(id, _)| *id == session_id).map(|(_, pane)| *pane)
                })
                .unwrap_or(first_pane);
            trees.insert(*workspace_id, cx.new(|_| PaneTree::from_root(root, active_pane)));
            focused.insert(*workspace_id, active_pane);
            for (session_id, pane_id) in pairs {
                sessions.insert(pane_id, session_id);
                visible.push(session_id);
            }
        }
        if trees.is_empty() {
            return Vec::new();
        }
        self.workspace = cx.new(|_| WorkspaceTree::from_tree(&pruned));
        self.trees = trees;
        self.focused = focused;
        self.sessions = sessions;
        self.pending.clear();
        self.pending_workspaces.clear();
        // Every region id comes out of the tree the server itself persisted,
        // so all of them may be named in a `MoveSession` or `CloseWorkspace`
        // from the first frame onward.
        self.adopted_server_workspace = true;
        self.server_workspaces = leaves.iter().map(|(workspace_id, ..)| *workspace_id).collect();
        visible
    }

    /// The region containing `pane`, or `None` once the pane has been closed.
    fn region_of(&self, pane: PaneId, cx: &App) -> Option<WorkspaceId> {
        self.trees
            .iter()
            .find_map(|(id, tree)| tree.read(cx).all_pane_ids().contains(&pane).then_some(*id))
    }

    /// The content rect `workspace_id` occupies inside `viewport`, minus a
    /// lower region's tab bar, so pane math and painted panes agree.
    fn region_rect(&self, workspace_id: WorkspaceId, viewport: Rect, cx: &App) -> Option<Rect> {
        let layout: &WindowLayout = self.workspace.read(cx).layout();
        layout.compute_workspace_rects(viewport).into_iter().find_map(|(id, rect)| {
            (id == workspace_id).then_some(Self::content_rect(
                rect,
                self.ci_strip(workspace_id),
                self.board_strip(workspace_id),
            ))
        })
    }
}

/// Serialize one region's pane split into the frozen wire shape, dropping panes
/// that have no session yet.
///
/// A split whose child prunes away collapses to its surviving child, so the
/// reported tree never carries a split with a missing side. Returns `None` when
/// the whole region is still waiting for its first session.
fn pane_node_to_wire(
    node: &LayoutNode,
    sessions: &HashMap<PaneId, SessionId>,
) -> Option<PaneTreeNode> {
    match node {
        LayoutNode::Leaf(pane_id) => {
            sessions.get(pane_id).map(|session_id| PaneTreeNode::Leaf { session_id: *session_id })
        }
        LayoutNode::Split { direction, ratio, first, second } => {
            let first = pane_node_to_wire(first, sessions);
            let second = pane_node_to_wire(second, sessions);
            match (first, second) {
                (Some(first), Some(second)) => Some(PaneTreeNode::Split {
                    direction: wire_direction(*direction),
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// The session of the first leaf of a serialized pane tree, which is the tab's
/// own identity on the wire.
fn first_session(node: &PaneTreeNode) -> SessionId {
    match node {
        PaneTreeNode::Leaf { session_id } => *session_id,
        PaneTreeNode::Split { first, .. } => first_session(first),
    }
}

/// Lower a client split direction onto its protocol twin.
const fn wire_direction(direction: SplitDirection) -> LayoutDirection {
    match direction {
        SplitDirection::Horizontal => LayoutDirection::Horizontal,
        SplitDirection::Vertical => LayoutDirection::Vertical,
    }
}

/// The pane split one persisted tab displays: its stored split tree, or a
/// single-pane leaf when the tab was serialized without one (the `None`
/// convention `node_to_tree` writes for unsplit tabs).
/// One region's wire payload: its ordered tabs, their pane trees, and which of
/// them is active.
pub struct RegionTabs {
    pub session_ids: Vec<SessionId>,
    pub pane_trees: Vec<Option<PaneTreeNode>>,
    pub active_tab_index: usize,
}

/// Build one region's tab payload from the window's strip and the region's live
/// pane split.
///
/// The GPUI shell holds ONE pane split per region and shows a tab by adopting it
/// into the focused pane, so the wire's "pane tree per tab" is expressed as: the
/// live split sits at the active tab's index and every other tab is a plain leaf
/// (`None`). [`wire_tab_pane_tree`] reads exactly that back, so a report and an
/// adopt round-trip to the same layout.
///
/// Every session the split shows is also a tab, so any that the strip has not
/// caught up with yet is appended rather than dropped — the report is the only
/// record the server keeps, and a session missing from it is a tab the next
/// connect will not restore.
fn region_tab_payload(
    workspace_id: WorkspaceId,
    tabs: &TabSessions,
    displayed: Option<PaneTreeNode>,
    active: Option<SessionId>,
) -> RegionTabs {
    let mut session_ids: Vec<SessionId> = tabs
        .region(workspace_id)
        .map(|region| region.tabs().iter().map(|tab| tab.session_id).collect())
        .unwrap_or_default();
    if let Some(displayed) = displayed.as_ref() {
        let mut shown = Vec::new();
        collect_pane_sessions(displayed, &mut shown);
        for session_id in shown {
            if !session_ids.contains(&session_id) {
                session_ids.push(session_id);
            }
        }
    }
    if session_ids.is_empty() {
        return RegionTabs { session_ids, pane_trees: Vec::new(), active_tab_index: 0 };
    }
    // The active tab is the one in the region's focused pane. A region whose
    // focus has not settled yet falls back to the split's first session, and
    // then to the first tab, so the index is always in range.
    let active_tab_index = active
        .or_else(|| displayed.as_ref().map(first_session))
        .and_then(|session_id| session_ids.iter().position(|id| *id == session_id))
        .unwrap_or(0);
    let mut pane_trees = vec![None; session_ids.len()];
    // A single-pane region needs no tree of its own: `wire_tab_pane_tree`
    // rebuilds a lone leaf from the tab's own session id.
    if matches!(displayed, Some(PaneTreeNode::Split { .. }))
        && let Some(slot) = pane_trees.get_mut(active_tab_index)
    {
        *slot = displayed;
    }
    RegionTabs { session_ids, pane_trees, active_tab_index }
}

/// Every session in a serialized pane split, left to right.
fn collect_pane_sessions(node: &PaneTreeNode, out: &mut Vec<SessionId>) {
    match node {
        PaneTreeNode::Leaf { session_id } => out.push(*session_id),
        PaneTreeNode::Split { first, second, .. } => {
            collect_pane_sessions(first, out);
            collect_pane_sessions(second, out);
        }
    }
}

/// Every tab of every region of a wire tree, in left-to-right region order.
///
/// This is the order the strip is restored to on a reconnect: the server's tree
/// is the only record of how the user arranged their tabs.
#[must_use]
pub fn wire_tree_tab_order(node: &WorkspaceTreeNode) -> Vec<SessionId> {
    let mut out = Vec::new();
    collect_wire_tab_order(node, &mut out);
    out
}

fn collect_wire_tab_order(node: &WorkspaceTreeNode, out: &mut Vec<SessionId>) {
    match node {
        WorkspaceTreeNode::Leaf { session_ids, .. } => out.extend(session_ids.iter().copied()),
        WorkspaceTreeNode::Split { first, second, .. } => {
            collect_wire_tab_order(first, out);
            collect_wire_tab_order(second, out);
        }
    }
}

fn wire_tab_pane_tree(
    session_ids: &[SessionId],
    pane_trees: &[Option<PaneTreeNode>],
    index: usize,
) -> Option<PaneTreeNode> {
    let session_id = *session_ids.get(index)?;
    Some(pane_trees.get(index).cloned().flatten().unwrap_or(PaneTreeNode::Leaf { session_id }))
}

/// Collect `(workspace, displayed tab's pane split)` for every leaf of a
/// pruned workspace tree, left to right.
fn wire_leaf_display_tabs(
    node: &WorkspaceTreeNode,
    out: &mut Vec<(WorkspaceId, PaneTreeNode, Option<SessionId>)>,
) {
    match node {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, pane_trees, active_tab_index } => {
            let index = (*active_tab_index).min(session_ids.len().saturating_sub(1));
            if let Some(displayed) = wire_tab_pane_tree(session_ids, pane_trees, index) {
                out.push((*workspace_id, displayed, session_ids.get(index).copied()));
            }
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            wire_leaf_display_tabs(first, out);
            wire_leaf_display_tabs(second, out);
        }
    }
}

/// Drop dead sessions from one persisted pane split. A split with one dead
/// side collapses to the survivor — the same rule [`pane_node_to_wire`]
/// applies to panes without sessions — and a fully dead subtree prunes to
/// `None`.
fn prune_pane_node(node: &PaneTreeNode, live: &HashSet<SessionId>) -> Option<PaneTreeNode> {
    match node {
        PaneTreeNode::Leaf { session_id } => {
            live.contains(session_id).then_some(PaneTreeNode::Leaf { session_id: *session_id })
        }
        PaneTreeNode::Split { direction, ratio, first, second } => {
            let first = prune_pane_node(first, live);
            let second = prune_pane_node(second, live);
            match (first, second) {
                (Some(first), Some(second)) => Some(PaneTreeNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// Drop dead sessions, emptied tabs, and emptied workspaces from a persisted
/// workspace tree.
///
/// Mirrors [`prune_pane_node`]'s collapse rule one level up: a workspace
/// split with one emptied side collapses to the survivor, and a tree that
/// prunes away entirely yields `None`. A surviving leaf keeps its displayed
/// tab when that tab survives, else falls back to its first surviving tab.
fn prune_workspace_node(
    node: &WorkspaceTreeNode,
    live: &HashSet<SessionId>,
) -> Option<WorkspaceTreeNode> {
    match node {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, pane_trees, active_tab_index } => {
            let mut kept_sessions = Vec::new();
            let mut kept_trees = Vec::new();
            let mut kept_active = 0;
            for (index, _) in session_ids.iter().enumerate() {
                let Some(tab) = wire_tab_pane_tree(session_ids, pane_trees, index) else {
                    continue;
                };
                let Some(kept) = prune_pane_node(&tab, live) else { continue };
                if index == *active_tab_index {
                    kept_active = kept_sessions.len();
                }
                kept_sessions.push(first_session(&kept));
                kept_trees.push(match kept {
                    PaneTreeNode::Leaf { .. } => None,
                    split @ PaneTreeNode::Split { .. } => Some(split),
                });
            }
            (!kept_sessions.is_empty()).then_some(WorkspaceTreeNode::Leaf {
                workspace_id: *workspace_id,
                session_ids: kept_sessions,
                pane_trees: kept_trees,
                active_tab_index: kept_active,
            })
        }
        WorkspaceTreeNode::Split { direction, ratio, first, second } => {
            let first = prune_workspace_node(first, live);
            let second = prune_workspace_node(second, live);
            match (first, second) {
                (Some(first), Some(second)) => Some(WorkspaceTreeNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use scribe_client::restore_replay::prepare_replay;
    use scribe_client::restore_state::{
        LaunchKind, LaunchRecord, PaneSnapshot, TabSnapshot, WorkspaceLayoutSnapshot,
        WorkspaceSnapshot,
    };

    use scribe_client::tab_session::TabEntry;

    use super::*;

    fn leaf(workspace_id: WorkspaceId, session_id: SessionId) -> WorkspaceTreeNode {
        WorkspaceTreeNode::Leaf {
            workspace_id,
            session_ids: vec![session_id],
            pane_trees: vec![None],
            active_tab_index: 0,
        }
    }

    /// A replayed region comes back rootless — [`WorkspaceSnapshot`] persists
    /// no project root, because the root is derived from a session's CWD and a
    /// stale one would outrank the server's own answer after a
    /// `workspaces.roots` edit. The server re-derives it from the replayed
    /// session's CWD (`WorkspaceManager::on_cwd_changed`, driven by the
    /// `Subscribe` every created pane sends) and this is the client half:
    /// the answer lands on the replayed region's slot.
    // @lat: [[client#GPUI Client Spike#Cold Restart Restore#Replayed workspace regains its project root]]
    #[gpui::test]
    fn replayed_workspace_regains_its_project_root(cx: &mut gpui::TestAppContext) {
        let workspace_id = WorkspaceId::new();
        let root = PathBuf::from("/home/dev/work/scribe");
        let snapshot = WindowRestoreState {
            version: 1,
            window_id: WindowId::new(),
            focused_workspace_id: workspace_id,
            root: WorkspaceLayoutSnapshot::Leaf { workspace_id },
            workspaces: vec![WorkspaceSnapshot {
                workspace_id,
                name: Some("scribe".to_owned()),
                accent_color: [0.4, 0.5, 0.6, 1.0],
                active_tab_index: 0,
                tabs: vec![TabSnapshot {
                    focused_launch_id: "launch-a".to_owned(),
                    pane_tree: PaneSnapshot::Leaf { launch_id: "launch-a".to_owned() },
                }],
            }],
            launches: vec![LaunchRecord {
                launch_id: "launch-a".to_owned(),
                cwd: Some(root.clone()),
                kind: LaunchKind::Shell,
                prompts: scribe_common::protocol::SessionPromptState::default(),
            }],
        };

        cx.update(|cx| {
            let mut shell = PaneShell::new([0.1, 0.2, 0.3, 1.0], cx);
            let launches = shell.adopt_restored(prepare_replay(&snapshot), cx);

            assert_eq!(launches.len(), 1, "the saved pane is queued for relaunch");
            assert_eq!(
                shell.focused_workspace_project_root(cx),
                None,
                "the snapshot carries no project root"
            );

            // What the server sends once the replayed session's CWD is matched
            // against the configured roots, parked by the reader's
            // `WorkspaceNamed` handling.
            let outcome = shell.apply_workspace_info(
                &WorkspaceInfo {
                    workspace_id,
                    name: Some("scribe".to_owned()),
                    accent: None,
                    project_root: Some(root.clone()),
                },
                cx,
            );

            assert_eq!(outcome, WorkspaceInfoOutcome::Updated, "the replayed region claims it");
            assert_eq!(
                shell.focused_workspace_project_root(cx).as_deref(),
                Some(Path::new("/home/dev/work/scribe"))
            );
        });
    }

    /// A top-row region keeps its full rect — its tabs live in the titlebar —
    /// while a stacked region cedes [`REGION_TAB_BAR_HEIGHT`] at its top to
    /// the in-region tab bar, and a region shorter than the bar cannot go
    /// negative.
    // @lat: [[test#GPUI Client Headless Suites#Lower regions reserve their tab bar]]
    #[test]
    fn lower_regions_reserve_their_tab_bar() {
        let top = Rect { x: 0.0, y: 0.0, width: 800.0, height: 300.0 };
        let kept = PaneShell::content_rect(top, 0.0, 0.0);
        assert!(
            (kept.y - top.y).abs() < f32::EPSILON
                && (kept.height - top.height).abs() < f32::EPSILON,
            "top-row rect passes through"
        );

        let lower = Rect { x: 0.0, y: 300.0, width: 800.0, height: 300.0 };
        let content = PaneShell::content_rect(lower, 0.0, 0.0);
        assert!((content.y - (300.0 + REGION_TAB_BAR_HEIGHT)).abs() < f32::EPSILON);
        assert!((content.height - (300.0 - REGION_TAB_BAR_HEIGHT)).abs() < f32::EPSILON);
        assert!((content.x - lower.x).abs() < f32::EPSILON);
        assert!((content.width - lower.width).abs() < f32::EPSILON);

        let sliver = Rect { x: 0.0, y: 300.0, width: 800.0, height: 10.0 };
        let clamped = PaneShell::content_rect(sliver, 0.0, 0.0);
        assert!(clamped.height >= 0.0, "a sliver region clamps instead of going negative");
    }

    // @lat: [[test#GPUI CI Run Bar#Band reflows only its workspace region]]
    #[test]
    fn ci_band_reflows_only_its_workspace_region() {
        let top = Rect { x: 400.0, y: 0.0, width: 400.0, height: 600.0 };
        let band = PaneShell::ci_rect(top, scribe_client::ci_bar::CI_BAR_HEIGHT)
            .expect("region has room for CI band");
        assert!((band.x - top.x).abs() < f32::EPSILON);
        assert!((band.y - top.y).abs() < f32::EPSILON);
        assert!((band.width - top.width).abs() < f32::EPSILON);
        assert!((band.height - scribe_client::ci_bar::CI_BAR_HEIGHT).abs() < f32::EPSILON);

        let content = PaneShell::content_rect(top, scribe_client::ci_bar::CI_BAR_HEIGHT, 0.0);
        assert!((content.x - top.x).abs() < f32::EPSILON);
        assert!((content.width - top.width).abs() < f32::EPSILON);
        assert!((content.y - scribe_client::ci_bar::CI_BAR_HEIGHT).abs() < f32::EPSILON);
        assert!(
            (content.height - (top.height - scribe_client::ci_bar::CI_BAR_HEIGHT)).abs()
                < f32::EPSILON
        );

        let expanded = scribe_client::ci_bar::CI_BAR_HEIGHT
            + scribe_client::ci_bar::CI_TRACE_BASE_HEIGHT
            + 3.0 * scribe_client::ci_bar::CI_TRACE_ROW_HEIGHT;
        let trace = PaneShell::ci_rect(top, expanded).expect("region has room for trace panel");
        assert!((trace.height - expanded).abs() < f32::EPSILON);
        let traced_content = PaneShell::content_rect(top, expanded, 0.0);
        assert!((traced_content.y - expanded).abs() < f32::EPSILON);
        assert!((traced_content.height - (top.height - expanded)).abs() < f32::EPSILON);
    }

    /// A pinned board takes its strip out of its own region's content, stacking
    /// with a lower region's tab bar and never widening past that region — the
    /// window-wide band it replaced pushed every region's panes down.
    // @lat: [[test#GPUI Client Headless Suites#A pinned board reserves only its own region]]
    #[test]
    fn a_pinned_board_reserves_only_its_own_region() {
        let board = 246.0;
        let top = Rect { x: 400.0, y: 0.0, width: 400.0, height: 600.0 };
        let with_board = PaneShell::content_rect(top, 0.0, board);
        assert!((with_board.y - board).abs() < f32::EPSILON);
        assert!((with_board.height - (600.0 - board)).abs() < f32::EPSILON);
        assert!(
            (with_board.x - top.x).abs() < f32::EPSILON
                && (with_board.width - top.width).abs() < f32::EPSILON,
            "the strip stays inside its own region's columns"
        );

        let lower = Rect { x: 0.0, y: 300.0, width: 800.0, height: 600.0 };
        let stacked = PaneShell::content_rect(lower, 0.0, board);
        assert!((stacked.y - (300.0 + REGION_TAB_BAR_HEIGHT + board)).abs() < f32::EPSILON);

        let shallow = PaneShell::content_rect(Rect { height: 100.0, ..top }, 0.0, board);
        assert!(shallow.height >= 0.0, "a region shorter than the board clamps to zero");

        assert!((PaneShell::reserved_board_height(600.0, board, 60.0) - board).abs() < 0.001);
        assert!(
            (PaneShell::reserved_board_height(180.0, board, 60.0) - 120.0).abs() < 0.001,
            "a short stacked region keeps its terminal reservation"
        );
    }

    /// The hot-reconnect adoption lowering preserves split orientation: a
    /// side-by-side (`Horizontal`) server tree must come back as regions with
    /// distinct x and equal y, never stacked. Exercises the exact wire→layout
    /// hops `adopt_server_tree` uses (prune → `WindowLayout::from_tree`).
    // @lat: [[test#GPUI Client Headless Suites#Hot-reconnect adoption lowering]]
    #[test]
    fn adoption_lowering_preserves_side_by_side_orientation() {
        let (ws_a, ws_b) = (WorkspaceId::new(), WorkspaceId::new());
        let (s_a, s_b) = (SessionId::new(), SessionId::new());
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(ws_a, s_a)),
            second: Box::new(leaf(ws_b, s_b)),
        };
        let live: HashSet<SessionId> = [s_a, s_b].into_iter().collect();

        let pruned = prune_workspace_node(&tree, &live).expect("both sessions live");
        assert_eq!(pruned, tree, "a fully live tree prunes to itself");

        let layout = WindowLayout::from_tree(&pruned);
        let viewport = Rect { x: 0.0, y: 0.0, width: 1000.0, height: 600.0 };
        let rects = layout.compute_workspace_rects(viewport);
        let rect_of = |id| {
            rects
                .iter()
                .find_map(|(ws, rect)| (*ws == id).then_some(*rect))
                .expect("region present")
        };
        let (a, b) = (rect_of(ws_a), rect_of(ws_b));
        assert!((a.y - b.y).abs() < 1.0, "side-by-side regions share y");
        assert!((b.x - 500.0).abs() < 1.0, "second region starts at half width");
        assert!((a.height - 600.0).abs() < 1.0, "regions span the full height");
    }

    // @lat: [[test#GPUI Client Headless Suites#Region reports every tab and which is active]]
    #[test]
    fn region_reports_every_tab_and_which_is_active() {
        let workspace_id = WorkspaceId::new();
        let other = WorkspaceId::new();
        let (first, second, third) = (SessionId::new(), SessionId::new(), SessionId::new());
        let mut strip = TabSessions::new();
        strip.reconcile(
            vec![
                TabEntry::new(first, workspace_id, "one".to_owned()),
                TabEntry::new(SessionId::new(), other, "elsewhere".to_owned()),
                TabEntry::new(second, workspace_id, "two".to_owned()),
                TabEntry::new(third, workspace_id, "three".to_owned()),
            ],
            None,
        );

        // One pane showing the middle tab: all three tabs are reported, in
        // strip order, and the active index names the one on screen.
        let displayed = PaneTreeNode::Leaf { session_id: second };
        let lone = region_tab_payload(workspace_id, &strip, Some(displayed), Some(second));
        assert_eq!(lone.session_ids, [first, second, third], "other regions' tabs excluded");
        assert_eq!(lone.active_tab_index, 1);
        assert!(lone.pane_trees.iter().all(Option::is_none), "a lone pane needs no tree");

        // A split region carries its tree at the active tab's index, and reading
        // it back reproduces the same split.
        let split = PaneTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneTreeNode::Leaf { session_id: first }),
            second: Box::new(PaneTreeNode::Leaf { session_id: third }),
        };
        let carried = region_tab_payload(workspace_id, &strip, Some(split.clone()), Some(third));
        assert_eq!(carried.active_tab_index, 2);
        assert_eq!(
            wire_tab_pane_tree(&carried.session_ids, &carried.pane_trees, carried.active_tab_index),
            Some(split),
            "the report round-trips back to the same split"
        );

        // A pane whose session the strip has not listed yet is still reported —
        // the tree is the only record the server keeps.
        let unlisted = SessionId::new();
        let appended = region_tab_payload(
            workspace_id,
            &strip,
            Some(PaneTreeNode::Leaf { session_id: unlisted }),
            Some(unlisted),
        );
        assert_eq!(appended.session_ids, [first, second, third, unlisted]);
        assert_eq!(appended.active_tab_index, 3);
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab order spans every region of the tree]]
    #[test]
    fn tab_order_spans_every_region_of_the_tree() {
        let (ws_a, ws_b) = (WorkspaceId::new(), WorkspaceId::new());
        let (a1, a2, b1) = (SessionId::new(), SessionId::new(), SessionId::new());
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_a,
                session_ids: vec![a1, a2],
                pane_trees: vec![None, None],
                active_tab_index: 1,
            }),
            second: Box::new(leaf(ws_b, b1)),
        };

        // Background tabs included, regions left to right: this is the order the
        // strip is restored to, so a tab that is not on screen still comes back
        // where the user put it.
        assert_eq!(wire_tree_tab_order(&tree), [a1, a2, b1]);
    }
}
