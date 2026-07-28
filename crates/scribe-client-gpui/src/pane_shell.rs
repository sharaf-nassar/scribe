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
use scribe_client_gpui::divider::{self, Divider};
use scribe_client_gpui::layout::{
    FocusDirection, LayoutNode, LayoutTree, PaneId, Rect, SplitDirection,
};
use scribe_client_gpui::pane_tree::PaneTree;
use scribe_client_gpui::restore_replay::{
    PaneRestore, RebuiltWindow, ReplayLaunch, new_shell_binding, snapshot_window_restore,
};
use scribe_client_gpui::restore_state::{LaunchBinding, WindowRestoreState};
use scribe_client_gpui::workspace_layout::{WindowLayout, WorkspaceSlot};
use scribe_client_gpui::workspace_tree::WorkspaceTree;
use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{LayoutDirection, PaneTreeNode, WorkspaceTreeNode},
};

/// Accent used when a region's slot cannot be read back out of the layout,
/// which only happens if a region is removed between two reads in one frame.
const FALLBACK_PANE_ACCENT: [f32; 4] = [0.0, 0.8, 0.7, 1.0];

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

/// Server-owned workspace metadata delivered by `ServerMessage::WorkspaceInfo`.
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

/// Build the slot edit that reapplies a restored region's name and accent.
fn apply_slot_metadata(name: Option<String>, accent: [f32; 4]) -> impl FnOnce(&mut WorkspaceSlot) {
    move |slot| {
        slot.name = name;
        slot.accent_color = accent;
    }
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

    /// The region containing `pane`, or `None` once the pane has been closed.
    pub fn region_for_pane(&self, pane: PaneId, cx: &App) -> Option<WorkspaceId> {
        self.region_of(pane, cx)
    }

    /// Serialize the window's live layout in the frozen `ReportWorkspaceTree`
    /// shape.
    ///
    /// The workspace topology comes from the [`WorkspaceTree`] that already
    /// owns it; the per-region payload is filled in from this shell's own pane
    /// trees, because the GPUI shell keeps panes in [`PaneTree`] entities rather
    /// than in the workspace model's tab list. A region maps to exactly one tab
    /// whose pane tree is the region's whole split, which is the wire shape the
    /// winit client restores from.
    ///
    /// Panes that are still waiting for a session are pruned instead of being
    /// serialized under a synthetic id: a reconnect must not restore a pane
    /// pointing at a session that never existed.
    pub fn wire_tree(&self, cx: &App) -> WorkspaceTreeNode {
        let topology = self.workspace.read(cx).to_tree();
        self.fill_regions(topology, cx)
    }

    /// Replace each leaf's (empty) tab payload with the region's live panes.
    fn fill_regions(&self, node: WorkspaceTreeNode, cx: &App) -> WorkspaceTreeNode {
        match node {
            WorkspaceTreeNode::Leaf { workspace_id, .. } => {
                let panes = self.trees.get(&workspace_id).and_then(|tree| {
                    pane_node_to_wire(tree.read(cx).tree().root(), &self.sessions)
                });
                let (session_ids, pane_trees) = match panes {
                    None => (Vec::new(), Vec::new()),
                    Some(PaneTreeNode::Leaf { session_id }) => (vec![session_id], vec![None]),
                    Some(split) => (vec![first_session(&split)], vec![Some(split)]),
                };
                WorkspaceTreeNode::Leaf {
                    workspace_id,
                    session_ids,
                    pane_trees,
                    active_tab_index: 0,
                }
            }
            WorkspaceTreeNode::Split { direction, ratio, first, second } => {
                WorkspaceTreeNode::Split {
                    direction,
                    ratio,
                    first: Box::new(self.fill_regions(*first, cx)),
                    second: Box::new(self.fill_regions(*second, cx)),
                }
            }
        }
    }

    /// The focused workspace region's ID.
    pub fn focused_workspace_id(&self, cx: &App) -> WorkspaceId {
        self.workspace.read(cx).focused_workspace_id()
    }

    /// The focused pane of the focused region.
    pub fn focused_pane(&self, cx: &App) -> Option<PaneId> {
        self.focused.get(&self.focused_workspace_id(cx)).copied()
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
    pub fn retain_sessions(&mut self, live: &HashSet<SessionId>, cx: &mut App) -> RetiredPanes {
        let dead: Vec<PaneId> = self
            .sessions
            .iter()
            .filter_map(|(pane, session)| (!live.contains(session)).then_some(*pane))
            .collect();
        let mut retired = RetiredPanes::default();
        for pane in dead {
            self.sessions.remove(&pane);
            // A sole pane in a sole region survives its session: the window
            // keeps its empty pane and the next attach fills it back in.
            if let Retired::Pane(Some(region)) = self.retire_pane(pane, cx) {
                retired.closed_regions.push(region);
            }
            retired.changed = true;
        }
        retired
    }

    /// Resolve every pane against `viewport` for one frame.
    pub fn placements(&self, viewport: Rect, cx: &App) -> Vec<PanePlacement> {
        let workspace = self.workspace.read(cx);
        let focused_workspace = workspace.focused_workspace_id();
        let mut out = Vec::new();
        for (workspace_id, region) in workspace.layout().compute_workspace_rects(viewport) {
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
                self.trees.get(&workspace_id).into_iter().flat_map(move |tree| {
                    divider::collect_dividers(tree.read(cx).tree().root(), region)
                })
            })
            .collect()
    }

    /// Set the split ratio containing `pane_id` and report whether it changed.
    pub fn set_pane_ratio(&mut self, pane_id: PaneId, ratio: f32, cx: &mut App) -> bool {
        let Some(workspace_id) = self.region_of(pane_id, cx) else { return false };
        let Some(tree) = self.trees.get(&workspace_id).cloned() else { return false };
        tree.update(cx, |pane_tree, ctx| pane_tree.set_ratio(pane_id, ratio, ctx))
    }

    /// Total number of live panes across every region.
    pub fn pane_count(&self, cx: &App) -> usize {
        self.trees.values().map(|tree| tree.read(cx).all_pane_ids().len()).sum()
    }

    /// Number of workspace regions the window is split into.
    pub fn region_count(&self, cx: &App) -> usize {
        self.workspace.read(cx).layout().workspace_count()
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
    pub fn restore_snapshot(
        &self,
        window_id: WindowId,
        bindings: &HashMap<SessionId, LaunchBinding>,
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
                bindings,
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
        bindings: &HashMap<SessionId, LaunchBinding>,
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
            let launch_binding =
                bindings.get(&session_id).cloned().unwrap_or_else(|| new_shell_binding(None));
            panes.insert(
                pane_id,
                PaneRestore {
                    session_id,
                    workspace_id,
                    cwd: launch_binding.fallback_cwd.clone(),
                    launch_binding,
                    first_prompt: None,
                    latest_prompt: None,
                    prompt_count: 0,
                    last_conversation_id: None,
                    grid: None,
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

    /// The region containing `pane`, or `None` once the pane has been closed.
    fn region_of(&self, pane: PaneId, cx: &App) -> Option<WorkspaceId> {
        self.trees
            .iter()
            .find_map(|(id, tree)| tree.read(cx).all_pane_ids().contains(&pane).then_some(*id))
    }

    /// The rect `workspace_id` occupies inside `viewport`.
    fn region_rect(&self, workspace_id: WorkspaceId, viewport: Rect, cx: &App) -> Option<Rect> {
        let layout: &WindowLayout = self.workspace.read(cx).layout();
        layout
            .compute_workspace_rects(viewport)
            .into_iter()
            .find_map(|(id, rect)| (id == workspace_id).then_some(rect))
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
