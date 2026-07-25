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
use scribe_client_gpui::layout::{FocusDirection, PaneId, Rect, SplitDirection};
use scribe_client_gpui::pane_tree::PaneTree;
use scribe_client_gpui::workspace_layout::{WindowLayout, WorkspaceSlot};
use scribe_client_gpui::workspace_tree::WorkspaceTree;
use scribe_common::ids::{SessionId, WorkspaceId};

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
    /// A pane (or a whole region) went away; these sessions lost their pane and
    /// must be closed on the server.
    Removed(Vec<SessionId>),
    /// The window is down to one pane in one region, so there is nothing to
    /// close at the layout level; the caller falls back to closing the tab.
    LastPane,
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
        }
    }

    /// Rename the root region to the workspace the server actually owns.
    ///
    /// Runs once: later regions are client-local layout until bead .66 wires
    /// `CreateWorkspace`, and renaming them would make the shell disagree with
    /// the server about which region a session lives in.
    pub fn adopt_server_workspace(&mut self, server_id: WorkspaceId, cx: &mut App) -> bool {
        if self.adopted_server_workspace {
            return false;
        }
        let current = self.focused_workspace_id(cx);
        if current == server_id {
            self.adopted_server_workspace = true;
            return false;
        }
        let renamed =
            self.workspace.update(cx, |tree, ctx| tree.set_workspace_id(current, server_id, ctx));
        if !renamed {
            return false;
        }
        self.adopted_server_workspace = true;
        if let Some(tree) = self.trees.remove(&current) {
            self.trees.insert(server_id, tree);
        }
        if let Some(pane) = self.focused.remove(&current) {
            self.focused.insert(server_id, pane);
        }
        true
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
        if !self.retire_pane(focused, cx) {
            return ClosedPane::LastPane;
        }
        ClosedPane::Removed(self.sessions.remove(&focused).into_iter().collect())
    }

    /// Remove `pane` from the layout, collapsing its region when it was the
    /// region's last pane and other regions remain.
    ///
    /// Returns `false` when nothing could be removed — the window is down to
    /// one pane in one region — which is the case both callers translate into
    /// "close the tab instead".
    fn retire_pane(&mut self, pane: PaneId, cx: &mut App) -> bool {
        let Some(workspace_id) = self.region_of(pane, cx) else { return false };
        let Some(pane_tree) = self.trees.get(&workspace_id).cloned() else { return false };
        if pane_tree.read(cx).all_pane_ids().len() > 1 {
            let next = pane_tree.read(cx).next_pane(pane);
            pane_tree.update(cx, |panes, ctx| panes.close(pane, ctx));
            self.refocus_after_close(workspace_id, pane, next);
            return true;
        }
        if self.workspace.read(cx).layout().workspace_count() <= 1 {
            return false;
        }
        self.trees.remove(&workspace_id);
        self.focused.remove(&workspace_id);
        self.workspace.update(cx, |window, ctx| window.remove_workspace(workspace_id, ctx));
        true
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
    /// Returns `true` when the layout changed, so the caller can republish pane
    /// sizes and repaint.
    pub fn retain_sessions(&mut self, live: &HashSet<SessionId>, cx: &mut App) -> bool {
        let dead: Vec<PaneId> = self
            .sessions
            .iter()
            .filter_map(|(pane, session)| (!live.contains(session)).then_some(*pane))
            .collect();
        let mut changed = false;
        for pane in dead {
            self.sessions.remove(&pane);
            // A sole pane in a sole region survives its session: the window
            // keeps its empty pane and the next attach fills it back in.
            self.retire_pane(pane, cx);
            changed = true;
        }
        changed
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

    /// Total number of live panes across every region.
    pub fn pane_count(&self, cx: &App) -> usize {
        self.trees.values().map(|tree| tree.read(cx).all_pane_ids().len()).sum()
    }

    /// Number of workspace regions the window is split into.
    pub fn region_count(&self, cx: &App) -> usize {
        self.workspace.read(cx).layout().workspace_count()
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
