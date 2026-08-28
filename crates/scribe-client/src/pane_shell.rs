//! The running client's live pane and workspace layout.
//!
//! This is the seam that turns the ported split-tree models into something the
//! shipped binary reaches. The window owns exactly one
//! [`WorkspaceTree`] — the window-level split of the grid area into workspace
//! regions — and one [`PaneTree`] per *strip tab*, keyed by the session that
//! tab was opened with. A region renders only its active tab's tree; the other
//! tabs' trees stay dormant until they are selected. Every pane hosts at most
//! one session, and the focused pane of the focused region is the pane
//! keystrokes, the status bar, and the tab strip follow.
//!
//! Tabs owning trees is what makes the invariant *a pane split never adds a
//! strip tab* hold: a split adds a pane to the tab's own tree and the session it
//! asks for is filed in [`TabSessions::insert_pane`], never in the strip. It is
//! also the shape the wire has always had — `WorkspaceTreeNode::Leaf` carries
//! one `pane_trees` entry per `session_ids` entry — so a report and an adoption
//! round-trip to the same layout.
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
use scribe_client::tab_session::{TabEntry, TabSessions};
use scribe_client::workspace_drag::WorkspaceDropZone;
use scribe_client::workspace_layout::{
    TabState, WindowLayout, WorkspaceDivider, WorkspaceSlot, pane_tree_to_layout_node,
};
use scribe_client::workspace_tree::WorkspaceTree;
use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{LayoutDirection, PaneTreeNode, WorkspaceTreeError, WorkspaceTreeNode},
};

/// Accent used when a region's slot cannot be read back out of the layout,
/// which only happens if a region is removed between two reads in one frame.
const FALLBACK_PANE_ACCENT: [f32; 4] = [0.0, 0.8, 0.7, 1.0];

/// Frame-local inputs for geometry inside workspace regions.
#[derive(Debug, Clone, Copy)]
pub struct RegionGeometry {
    /// Grid-area rectangle every workspace split resolves against.
    pub viewport: Rect,
    /// Live height lower regions reserve for their tab row.
    pub tab_bar_height: f32,
}

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

/// The result of dropping panes whose sessions have exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetiredPanes {
    /// Whether the layout actually changed, so the caller republishes sizes.
    pub changed: bool,
    /// Server-minted regions that collapsed because their last pane went away.
    pub closed_regions: Vec<WorkspaceId>,
    /// Pane sessions that became their tab's new anchor because the session the
    /// tab was keyed by exited. The caller gives each one a strip entry.
    pub promoted_tabs: Vec<SessionId>,
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

/// The live state a restore snapshot reads but the shell does not own: the
/// window's tab strip, the restore runtime's launch bindings, and the
/// IPC-written prompt history. All three are handed in together rather than
/// looked up per pane.
pub struct SnapshotSources<'a> {
    pub tabs: &'a TabSessions,
    pub bindings: &'a HashMap<SessionId, LaunchBinding>,
    pub prompts: &'a HashMap<SessionId, PromptBarData>,
}

/// The per-tab tables a whole-shell rebuild assembles before installing them.
///
/// Both rebuild paths — the cold-restart replay and the hot-reconnect tree
/// adoption — fill exactly these four, so they travel as one value rather than
/// as four parallel locals threaded through two nested loops.
#[derive(Default)]
struct TabTrees {
    trees: HashMap<SessionId, Entity<PaneTree>>,
    tab_regions: HashMap<SessionId, WorkspaceId>,
    shown_tabs: HashMap<WorkspaceId, SessionId>,
    focused: HashMap<SessionId, PaneId>,
}

impl TabTrees {
    fn insert(
        &mut self,
        workspace_id: WorkspaceId,
        tab: SessionId,
        tree: Entity<PaneTree>,
        focused_pane: PaneId,
    ) {
        self.tab_regions.insert(tab, workspace_id);
        self.focused.insert(tab, focused_pane);
        self.trees.insert(tab, tree);
    }

    /// Mark `tab` as the tree `workspace_id`'s region renders.
    fn show(&mut self, workspace_id: WorkspaceId, tab: SessionId) {
        self.shown_tabs.insert(workspace_id, tab);
    }

    /// Rebuild one saved tab's tree, keyed by the placeholder session id its
    /// replayed `CreateSession` answer replaces.
    fn restore_tab(
        &mut self,
        workspace_id: WorkspaceId,
        tab: &mut TabState,
        shown: bool,
        cx: &mut App,
    ) {
        let placeholder = tab.session_id;
        let wanted_focus = tab.focused_pane;
        let pane_layout = std::mem::replace(&mut tab.pane_layout, LayoutTree::new());
        let tree = cx.new(|_| PaneTree::from_tree(pane_layout));
        let panes = tree.read(cx);
        let focused_pane = if panes.all_pane_ids().contains(&wanted_focus) {
            wanted_focus
        } else {
            panes.initial_pane_id()
        };
        self.insert(workspace_id, placeholder, tree, focused_pane);
        if shown {
            self.show(workspace_id, placeholder);
        }
    }

    /// Rebuild one tab's tree from the server's wire split, returning its
    /// `(session, pane)` pairs in layout order.
    ///
    /// Focus lands on the pane showing the tab's own session, so a restored
    /// split comes back typed-into where the user left it rather than always in
    /// its leftmost pane.
    fn adopt_tab(
        &mut self,
        workspace_id: WorkspaceId,
        tab: SessionId,
        pane_tree: &PaneTreeNode,
        cx: &mut App,
    ) -> Vec<(SessionId, PaneId)> {
        let (root, pairs) = pane_tree_to_layout_node(pane_tree);
        let Some(&(_, first_pane)) = pairs.first() else { return Vec::new() };
        let focused_pane = pairs
            .iter()
            .find_map(|(session_id, pane)| (*session_id == tab).then_some(*pane))
            .unwrap_or(first_pane);
        let tree = cx.new(|_| PaneTree::from_root(root, focused_pane));
        self.insert(workspace_id, tab, tree, focused_pane);
        pairs
    }
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
    /// One pane split tree per strip tab, anchored by that tab's session.
    trees: HashMap<SessionId, Entity<PaneTree>>,
    /// The workspace each tab tree belongs to.
    tab_regions: HashMap<SessionId, WorkspaceId>,
    /// The tab each region is currently rendering.
    shown_tabs: HashMap<WorkspaceId, SessionId>,
    /// The focused pane in each tab tree, so switching tabs restores its own
    /// pane focus rather than inheriting the tab it replaced.
    focused: HashMap<SessionId, PaneId>,
    /// A region that has not received its first strip-tab session yet.
    pending_regions: HashMap<WorkspaceId, Entity<PaneTree>>,
    /// Restored tab roots waiting for their FIFO `CreateSession` answers.
    pending_tab_roots: HashMap<WorkspaceId, VecDeque<SessionId>>,
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
        Self {
            workspace,
            trees: HashMap::new(),
            tab_regions: HashMap::new(),
            shown_tabs: HashMap::new(),
            focused: HashMap::new(),
            pending_regions: HashMap::from([(workspace_id, tree)]),
            pending_tab_roots: HashMap::new(),
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
            if self.pending_regions.contains_key(&local_id)
                && self.rename_region(local_id, server_id, cx)
            {
                return true;
            }
        }
        false
    }

    /// Move a region — its slot and every tab tree it owns — from `old_id` to
    /// `new_id`, keeping server-known state in step.
    fn rename_region(&mut self, old_id: WorkspaceId, new_id: WorkspaceId, cx: &mut App) -> bool {
        if !self.workspace.update(cx, |tree, ctx| tree.set_workspace_id(old_id, new_id, ctx)) {
            return false;
        }
        if let Some(tree) = self.pending_regions.remove(&old_id) {
            self.pending_regions.insert(new_id, tree);
        }
        if let Some(tab) = self.shown_tabs.remove(&old_id) {
            self.shown_tabs.insert(new_id, tab);
        }
        if let Some(roots) = self.pending_tab_roots.remove(&old_id) {
            self.pending_tab_roots.insert(new_id, roots);
        }
        for workspace_id in self.tab_regions.values_mut() {
            if *workspace_id == old_id {
                *workspace_id = new_id;
            }
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
    pub fn has_region(&self, workspace_id: WorkspaceId, cx: &App) -> bool {
        self.workspace.read(cx).find_workspace(workspace_id).is_some()
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
                let region = region_tab_payload(
                    workspace_id,
                    tabs,
                    self.tab_pane_trees(workspace_id, tabs, cx),
                );
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

    /// Serialize one region's tabs, in strip order, as each tab's own pane
    /// split. A tab whose tree holds a single session serializes to a bare leaf.
    fn tab_pane_trees(
        &self,
        workspace_id: WorkspaceId,
        tabs: &TabSessions,
        cx: &App,
    ) -> Vec<Option<PaneTreeNode>> {
        let Some(region) = tabs.region(workspace_id) else { return Vec::new() };
        region
            .tabs()
            .iter()
            .map(|tab| {
                let tree = self.trees.get(&tab.session_id)?;
                pane_node_to_wire(tree.read(cx).tree().root(), &self.sessions)
            })
            .collect()
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

    /// Top-row regions and their left edges, including regions with no tabs.
    pub fn top_region_left_edges(&self, viewport: Rect, cx: &App) -> Vec<(WorkspaceId, f32)> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .filter(|(_, rect)| !Self::is_lower_region(*rect))
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
    fn content_rect(rect: Rect, tab_bar_height: f32, ci: f32, board: f32) -> Rect {
        let bar = if Self::is_lower_region(rect) { tab_bar_height } else { 0.0 };
        let reserved = (bar + ci + board).min(rect.height);
        Rect { x: rect.x, y: rect.y + reserved, width: rect.width, height: rect.height - reserved }
    }

    /// The collapsed CI strip inside one raw region rect, directly below tabs.
    fn ci_rect(rect: Rect, tab_bar_height: f32, strip: f32) -> Option<Rect> {
        let tab = if Self::is_lower_region(rect) { tab_bar_height } else { 0.0 };
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

    /// One band's slice of a region's CI strip: `offset` below the strip's top,
    /// `height` tall, clipped to what the strip has left. A band with no room
    /// resolves to `None` rather than painting over its neighbor or the panes.
    fn ci_band_rect(strip: Rect, offset: f32, height: f32) -> Option<Rect> {
        let height = height.min(strip.height - offset);
        (offset >= 0.0 && height > 0.0).then_some(Rect {
            x: strip.x,
            y: strip.y + offset,
            width: strip.width,
            height,
        })
    }

    /// Resolve one visible CI band against the region layout. Stacked bands
    /// share the region's reserved strip: `band` is one band's `(offset from
    /// the strip's top, height)`.
    pub fn ci_bar_rect(
        &self,
        workspace_id: WorkspaceId,
        band: (f32, f32),
        geometry: RegionGeometry,
        cx: &App,
    ) -> Option<Rect> {
        let (offset, height) = band;
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(geometry.viewport)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .and_then(|(_, rect)| {
                Self::ci_rect(rect, geometry.tab_bar_height, self.ci_strip(workspace_id))
            })
            .and_then(|strip| Self::ci_band_rect(strip, offset, height))
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
        geometry: RegionGeometry,
        cx: &App,
    ) -> Option<Rect> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(geometry.viewport)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .map(|(_, rect)| {
                Self::content_rect(rect, geometry.tab_bar_height, self.ci_strip(workspace_id), 0.0)
            })
            .map(|content| Rect { height: height.min(content.height), ..content })
    }

    /// A pinned board rect capped before `terminal_reservation`, so stored
    /// height remains a preference while a short stacked region still keeps
    /// its terminal recoverable.
    pub fn reserved_board_rect(
        &self,
        workspace_id: WorkspaceId,
        reservation: BoardReservation,
        geometry: RegionGeometry,
        cx: &App,
    ) -> Option<Rect> {
        let content = self.board_rect(workspace_id, f32::MAX, geometry, cx)?;
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
        self.workspace_rects(viewport, cx)
            .into_iter()
            .find(|(id, _)| *id == workspace_id)
            .map(|(_, rect)| rect)
    }

    /// Every workspace region's raw bounds in tree order.
    pub fn workspace_rects(&self, viewport: Rect, cx: &App) -> Vec<(WorkspaceId, Rect)> {
        self.workspace.read(cx).layout().compute_workspace_rects(viewport)
    }

    /// The tab-bar strip each lower region reserves at its top, in region
    /// left-to-right, top-to-bottom order.
    pub fn region_bar_rects(
        &self,
        viewport: Rect,
        tab_bar_height: f32,
        cx: &App,
    ) -> Vec<(WorkspaceId, Rect)> {
        self.workspace
            .read(cx)
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .filter(|(_, rect)| Self::is_lower_region(*rect))
            .map(|(workspace_id, rect)| {
                let bar = tab_bar_height.min(rect.height);
                (workspace_id, Rect { x: rect.x, y: rect.y, width: rect.width, height: bar })
            })
            .collect()
    }

    /// The strip tab `workspace_id`'s region is rendering.
    pub fn region_shown_session(&self, workspace_id: WorkspaceId) -> Option<SessionId> {
        self.shown_tabs.get(&workspace_id).copied()
    }

    /// Synchronize the dormant per-tab trees with the reader-owned strip.
    ///
    /// A fresh tab claims the region's provisional root tree; every later tab
    /// starts with its own one-pane tree. A restored tab root is re-keyed from
    /// its placeholder to the FIFO-created session when the strip first sees it.
    pub fn sync_tabs(&mut self, tabs: &TabSessions, cx: &mut App) -> bool {
        let mut changed = false;
        for region in tabs.regions() {
            // A strip legitimately holds tabs for workspaces this window shows
            // no region for — a reattach after reconnect, a strip that outlived
            // its region — and those tabs own no tree here.
            if self.workspace.read(cx).find_workspace(region.workspace_id).is_none() {
                continue;
            }
            for tab in region.tabs() {
                changed |= self.ensure_tab_tree(region.workspace_id, tab.session_id, cx);
            }
            if let Some(active) = region.active_session()
                && self.tab_regions.get(&active) == Some(&region.workspace_id)
                && self.shown_tabs.insert(region.workspace_id, active) != Some(active)
            {
                changed = true;
            }
        }
        self.shown_tabs.retain(|workspace_id, tab| self.tab_regions.get(tab) == Some(workspace_id));
        changed |= self.drop_closed_tab_trees(tabs, cx);
        changed
    }

    /// Forget the tree of a tab the strip has dropped and whose panes have all
    /// exited, so a closed tab cannot be rendered or reported again.
    ///
    /// A tab the strip has not caught up with yet keeps its tree: a fresh tab's
    /// root pane has no session for a frame, and a restored tab's tree is still
    /// keyed by the placeholder its `CreateSession` answer will replace.
    fn drop_closed_tab_trees(&mut self, tabs: &TabSessions, cx: &App) -> bool {
        let restoring: HashSet<SessionId> =
            self.pending_tab_roots.values().flatten().copied().collect();
        let stale: Vec<SessionId> = self
            .trees
            .keys()
            .copied()
            .filter(|tab| {
                !restoring.contains(tab)
                    && tabs.region_of_tab(*tab).is_none()
                    && self.tree_sessions(*tab, cx).is_empty()
            })
            .collect();
        for tab in &stale {
            self.trees.remove(tab);
            self.focused.remove(tab);
            self.tab_regions.remove(tab);
        }
        self.shown_tabs.retain(|_, tab| self.tab_regions.contains_key(tab));
        !stale.is_empty()
    }

    /// Show one tab's complete pane tree and return every pane session it owns.
    pub fn show_tab(
        &mut self,
        workspace_id: WorkspaceId,
        tab: SessionId,
        cx: &mut App,
    ) -> Vec<SessionId> {
        self.ensure_tab_tree(workspace_id, tab, cx);
        self.shown_tabs.insert(workspace_id, tab);
        if self.focused_workspace_id(cx) != workspace_id {
            self.workspace.update(cx, |tree, ctx| tree.set_focused_workspace(workspace_id, ctx));
        }
        self.tree_sessions(tab, cx)
    }

    /// `workspace_id`'s region accent, for the strip's per-workspace badges.
    pub fn workspace_accent(&self, workspace_id: WorkspaceId, cx: &App) -> [f32; 4] {
        self.workspace
            .read(cx)
            .find_workspace(workspace_id)
            .map_or(FALLBACK_PANE_ACCENT, |slot| slot.accent_color)
    }

    /// The focused pane of the focused region's active tab.
    pub fn focused_pane(&self, cx: &App) -> Option<PaneId> {
        self.region_focused_pane(self.focused_workspace_id(cx))
    }

    /// The focused pane of `workspace_id`'s active tab.
    pub fn region_focused_pane(&self, workspace_id: WorkspaceId) -> Option<PaneId> {
        self.shown_tabs.get(&workspace_id).and_then(|tab| self.focused.get(tab)).copied()
    }

    /// The session the focused pane is showing.
    pub fn focused_session(&self, cx: &App) -> Option<SessionId> {
        self.focused_pane(cx).and_then(|pane| self.sessions.get(&pane).copied())
    }

    /// Every session in the trees the regions are currently rendering.
    pub fn shown_sessions(&self, cx: &App) -> HashSet<SessionId> {
        self.shown_tabs.values().flat_map(|tab| self.tree_sessions(*tab, cx)).collect()
    }

    /// Every session currently assigned to any tab tree, including dormant tabs.
    pub fn assigned_sessions(&self) -> HashSet<SessionId> {
        self.sessions.values().copied().collect()
    }

    /// The `(region, pane)` showing `session_id`, if it is in an active tree.
    pub fn pane_for_session(
        &self,
        session_id: SessionId,
        cx: &App,
    ) -> Option<(WorkspaceId, PaneId)> {
        let pane =
            self.sessions.iter().find_map(|(pane, sid)| (*sid == session_id).then_some(*pane))?;
        let tab = self.tab_for_pane(pane, cx)?;
        let workspace = self.tab_regions.get(&tab).copied()?;
        (self.shown_tabs.get(&workspace) == Some(&tab)).then_some((workspace, pane))
    }

    /// Point `pane` at `session_id`, returning the session it displaced.
    pub fn assign_session(
        &mut self,
        pane: PaneId,
        session_id: SessionId,
        cx: &App,
    ) -> Option<SessionId> {
        if self.tab_for_pane(pane, cx).is_none()
            && let Some(workspace_id) =
                self.pending_regions.iter().find_map(|(workspace_id, tree)| {
                    tree.read(cx).all_pane_ids().contains(&pane).then_some(*workspace_id)
                })
            && let Some(tree) = self.pending_regions.remove(&workspace_id)
        {
            self.trees.insert(session_id, tree);
            self.tab_regions.insert(session_id, workspace_id);
            self.shown_tabs.insert(workspace_id, session_id);
            self.focused.entry(session_id).or_insert(pane);
        }
        self.sessions.insert(pane, session_id).filter(|prev| *prev != session_id)
    }

    /// Focus `pane` inside `workspace_id`, making that region focused too.
    pub fn focus_pane(&mut self, workspace_id: WorkspaceId, pane: PaneId, cx: &mut App) {
        if let Some(tab) = self.tab_for_pane(pane, cx) {
            self.shown_tabs.insert(workspace_id, tab);
            self.focused.insert(tab, pane);
        }
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
            if self.tab_for_pane(pane, cx).is_some()
                || self
                    .pending_regions
                    .values()
                    .any(|tree| tree.read(cx).all_pane_ids().contains(&pane))
            {
                return Some(pane);
            }
        }
        None
    }

    /// Split the focused pane in its active tab and queue the new pane.
    pub fn split_focused_pane(
        &mut self,
        direction: SplitDirection,
        cx: &mut App,
    ) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let tab = *self.shown_tabs.get(&workspace_id)?;
        let focused = *self.focused.get(&tab)?;
        let tree = self.trees.get(&tab)?.clone();
        let new_pane =
            tree.update(cx, |pane_tree, ctx| pane_tree.split(focused, direction, ctx))?;
        self.focused.insert(tab, new_pane);
        self.pending.push_back(new_pane);
        Some(new_pane)
    }

    /// Close a focused pane. A tab's sole pane falls through to close-tab.
    pub fn close_focused_pane(&mut self, cx: &mut App) -> ClosedPane {
        let Some(focused) = self.focused_pane(cx) else { return ClosedPane::LastPane };
        let Some(tab) = self.tab_for_pane(focused, cx) else { return ClosedPane::LastPane };
        let Some(tree) = self.trees.get(&tab).cloned() else { return ClosedPane::LastPane };
        if tree.read(cx).all_pane_ids().len() <= 1 {
            return ClosedPane::LastPane;
        }
        let next = tree.read(cx).next_pane(focused);
        tree.update(cx, |panes, ctx| panes.close(focused, ctx));
        self.focused.insert(tab, next);
        ClosedPane::Removed {
            sessions: self.sessions.remove(&focused).into_iter().collect(),
            closed_region: None,
        }
    }

    /// Return every session in the tab that `session_id` identifies.
    pub fn close_tab_sessions(&self, session_id: SessionId, cx: &App) -> Vec<SessionId> {
        self.tree_sessions(session_id, cx)
    }

    /// Cycle focus inside the focused region's active tab.
    pub fn focus_next_pane(&mut self, cx: &mut App) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let tab = *self.shown_tabs.get(&workspace_id)?;
        let focused = *self.focused.get(&tab)?;
        let tree = self.trees.get(&tab)?;
        let next = tree.read(cx).next_pane(focused);
        if next == focused {
            return None;
        }
        self.focused.insert(tab, next);
        Some(next)
    }

    /// Move pane focus spatially inside the focused region's active tab.
    pub fn focus_pane_in_direction(
        &mut self,
        direction: FocusDirection,
        viewport: Rect,
        tab_bar_height: f32,
        cx: &mut App,
    ) -> Option<PaneId> {
        let workspace_id = self.focused_workspace_id(cx);
        let tab = *self.shown_tabs.get(&workspace_id)?;
        let focused = *self.focused.get(&tab)?;
        let region = self.region_rect(workspace_id, viewport, tab_bar_height, cx)?;
        let tree = self.trees.get(&tab)?;
        let pane_tree = tree.read(cx);
        let rects = pane_tree.compute_rects(region);
        let next = pane_tree.find_pane_in_direction(focused, direction, &rects)?;
        if next == focused {
            return None;
        }
        self.focused.insert(tab, next);
        Some(next)
    }

    /// Split the window into a new workspace region and reserve its first tab.
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
        self.pending.push_back(tree.read(cx).initial_pane_id());
        self.pending_regions.insert(new_id, tree);
        self.expect_server_workspace(new_id);
        Some(new_id)
    }

    /// Ensure `tab` owns a tree, claiming a provisional or restored root when
    /// the reader has just learned the server session ID.
    ///
    /// Three sources, in order: a restored tab's tree, still keyed by the
    /// placeholder its `CreateSession` answer replaces; the provisional tree a
    /// fresh region reserved before it had a tab; else a new one-pane tree. The
    /// tree's panes are left session-less — every pane takes its session through
    /// [`PaneShell::assign_session`] so the caller can stream it and hand it any
    /// restored prompt history.
    fn ensure_tab_tree(&mut self, workspace_id: WorkspaceId, tab: SessionId, cx: &mut App) -> bool {
        if self.trees.contains_key(&tab) {
            return false;
        }
        let tree = self
            .pending_tab_roots
            .get_mut(&workspace_id)
            .and_then(VecDeque::pop_front)
            .and_then(|placeholder| {
                self.tab_regions.remove(&placeholder);
                let focus = self.focused.remove(&placeholder);
                let tree = self.trees.remove(&placeholder)?;
                if let Some(focus) = focus {
                    self.focused.insert(tab, focus);
                }
                Some(tree)
            })
            .or_else(|| self.pending_regions.remove(&workspace_id))
            .unwrap_or_else(|| cx.new(|_| PaneTree::new()));
        let root = tree.read(cx).initial_pane_id();
        self.focused.entry(tab).or_insert(root);
        self.tab_regions.insert(tab, workspace_id);
        self.trees.insert(tab, tree);
        true
    }

    /// Install the per-tab tables a whole-shell rebuild assembled.
    fn install_tab_trees(&mut self, tab_trees: TabTrees) {
        self.trees = tab_trees.trees;
        self.tab_regions = tab_trees.tab_regions;
        self.shown_tabs = tab_trees.shown_tabs;
        self.focused = tab_trees.focused;
    }

    /// Move a tab's tree, focus, and region from `old` onto `new`.
    fn rekey_tab(&mut self, old: SessionId, new: SessionId) {
        if let Some(tree) = self.trees.remove(&old) {
            self.trees.insert(new, tree);
        }
        if let Some(focus) = self.focused.remove(&old) {
            self.focused.insert(new, focus);
        }
        if let Some(workspace_id) = self.tab_regions.remove(&old) {
            self.tab_regions.insert(new, workspace_id);
        }
        for tab in self.shown_tabs.values_mut() {
            if *tab == old {
                *tab = new;
            }
        }
    }

    fn tab_for_pane(&self, pane: PaneId, cx: &App) -> Option<SessionId> {
        self.trees
            .iter()
            .find_map(|(tab, tree)| tree.read(cx).all_pane_ids().contains(&pane).then_some(*tab))
    }

    fn tree_sessions(&self, tab: SessionId, cx: &App) -> Vec<SessionId> {
        self.trees
            .get(&tab)
            .into_iter()
            .flat_map(|tree| tree.read(cx).all_pane_ids())
            .filter_map(|pane| self.sessions.get(&pane).copied())
            .collect()
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
    /// keeps that tree empty instead of collapsing, so the region survives long
    /// enough for [`Self::sync_tabs`] to swap it onto the tab the strip
    /// refocused and a hidden tab can never be orphaned by its shown sibling's
    /// exit.
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
            self.retire_pane(pane, workspaces_with_tabs, &mut retired, cx);
        }
        retired
    }

    /// Drop one pane whose session exited, re-anchoring or emptying its tab.
    fn retire_pane(
        &mut self,
        pane: PaneId,
        workspaces_with_tabs: &HashSet<WorkspaceId>,
        retired: &mut RetiredPanes,
        cx: &mut App,
    ) {
        let Some(tab) = self.tab_for_pane(pane, cx) else { return };
        let workspace_id = self.tab_regions.get(&tab).copied();
        let exited = self.sessions.remove(&pane);
        let Some(tree) = self.trees.get(&tab).cloned() else { return };
        if tree.read(cx).all_pane_ids().len() > 1 {
            let next = tree.read(cx).next_pane(pane);
            tree.update(cx, |panes, ctx| panes.close(pane, ctx));
            if self.focused.get(&tab) == Some(&pane) {
                self.focused.insert(tab, next);
            }
        }
        // A tab is keyed by the session it was opened with. When that session
        // exits while the splits it made are still running, the tab re-anchors
        // onto a surviving pane rather than stranding live sessions in a tree
        // the strip can no longer reach — the same rule `prune_workspace_node`
        // applies to the persisted tree.
        let tab = match self.tree_sessions(tab, cx).first().copied() {
            Some(anchor) if exited == Some(tab) => {
                self.rekey_tab(tab, anchor);
                retired.promoted_tabs.push(anchor);
                anchor
            }
            _ => tab,
        };
        retired.changed = true;
        let Some(workspace_id) = workspace_id else { return };
        if workspaces_with_tabs.contains(&workspace_id) || !self.tree_sessions(tab, cx).is_empty() {
            return;
        }
        self.trees.remove(&tab);
        self.focused.remove(&tab);
        self.tab_regions.remove(&tab);
        self.shown_tabs.remove(&workspace_id);
        if self.tab_regions.values().any(|region| *region == workspace_id)
            || self.workspace.read(cx).layout().workspace_count() <= 1
        {
            return;
        }
        self.workspace.update(cx, |window, ctx| window.remove_workspace(workspace_id, ctx));
        self.pending_workspaces.retain(|id| *id != workspace_id);
        // Only a region the server minted can be closed on the server; one that
        // is still waiting for its `WorkspaceInfo` names an id the server has
        // never seen.
        if let Some(region) = self.server_workspaces.take(&workspace_id) {
            retired.closed_regions.push(region);
        }
    }

    /// Every rendered pane with no session that is not already queued for one,
    /// paired with the region and the tab that owns it — the panes the reconcile
    /// refill hands their own tab's session.
    pub fn empty_unpending_panes(&self, cx: &App) -> Vec<(WorkspaceId, SessionId, PaneId)> {
        self.shown_tabs
            .iter()
            .flat_map(|(workspace_id, tab)| {
                self.trees
                    .get(tab)
                    .into_iter()
                    .flat_map(|tree| tree.read(cx).all_pane_ids())
                    .filter(|pane| {
                        !self.sessions.contains_key(pane) && !self.pending.contains(pane)
                    })
                    .map(|pane| (*workspace_id, *tab, pane))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Resolve every pane against `viewport` for one frame.
    pub fn placements(&self, viewport: Rect, tab_bar_height: f32, cx: &App) -> Vec<PanePlacement> {
        let workspace = self.workspace.read(cx);
        let focused_workspace = workspace.focused_workspace_id();
        let mut out = Vec::new();
        for (workspace_id, region) in workspace.layout().compute_workspace_rects(viewport) {
            let region = Self::content_rect(
                region,
                tab_bar_height,
                self.ci_strip(workspace_id),
                self.board_strip(workspace_id),
            );
            let Some(tab) = self.shown_tabs.get(&workspace_id) else { continue };
            let Some(tree) = self.trees.get(tab) else { continue };
            let accent = workspace
                .find_workspace(workspace_id)
                .map_or(FALLBACK_PANE_ACCENT, |slot: &WorkspaceSlot| slot.accent_color);
            let focused_pane = self.focused.get(tab).copied();
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
    pub fn dividers(&self, viewport: Rect, tab_bar_height: f32, cx: &App) -> Vec<Divider> {
        let workspace = self.workspace.read(cx);
        workspace
            .layout()
            .compute_workspace_rects(viewport)
            .into_iter()
            .flat_map(|(workspace_id, region)| {
                let region = Self::content_rect(
                    region,
                    tab_bar_height,
                    self.ci_strip(workspace_id),
                    self.board_strip(workspace_id),
                );
                self.shown_tabs
                    .get(&workspace_id)
                    .and_then(|tab| self.trees.get(tab))
                    .into_iter()
                    .flat_map(move |tree| {
                        divider::collect_dividers(tree.read(cx).tree().root(), region)
                    })
            })
            .collect()
    }

    /// Resolve every workspace-region divider against the grid viewport.
    pub fn workspace_dividers(&self, viewport: Rect, cx: &App) -> Vec<WorkspaceDivider> {
        self.workspace.read(cx).layout().collect_workspace_dividers(viewport)
    }

    /// Apply an in-window workspace drag through the shared tree operations.
    pub fn rearrange_workspace(
        &mut self,
        source_workspace_id: WorkspaceId,
        target_workspace_id: WorkspaceId,
        zone: WorkspaceDropZone,
        cx: &mut App,
    ) -> Result<bool, WorkspaceTreeError> {
        self.workspace.update(cx, |tree, ctx| {
            tree.rearrange_workspace(source_workspace_id, target_workspace_id, zone, ctx)
        })
    }

    /// Move the focused workspace through the same directional edge operation
    /// used by a workspace-pill drag.
    pub fn move_focused_workspace_in_direction(
        &mut self,
        direction: FocusDirection,
        viewport: Rect,
        cx: &mut App,
    ) -> Result<bool, WorkspaceTreeError> {
        self.workspace.update(cx, |tree, ctx| {
            tree.move_focused_workspace_in_direction(direction, viewport, ctx)
        })
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
        let Some(tab) = self.tab_for_pane(pane_id, cx) else { return false };
        let Some(tree) = self.trees.get(&tab).cloned() else { return false };
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
        self.shown_tabs
            .values()
            .filter_map(|tab| self.trees.get(tab))
            .map(|tree| tree.read(cx).all_pane_ids().len())
            .sum()
    }

    /// Number of panes in the focused region's active tab.
    pub fn focused_region_pane_count(&self, cx: &App) -> usize {
        self.shown_tabs
            .get(&self.focused_workspace_id(cx))
            .and_then(|tab| self.trees.get(tab))
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
        sources: &SnapshotSources<'_>,
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
                sources,
                cx,
            );
        }
        layout.set_focused_workspace(workspace.focused_workspace_id());
        snapshot_window_restore(window_id, &layout, &panes)
    }

    /// Copy every tab's pane tree into the scratch layout and record its panes.
    fn snapshot_region(
        &self,
        workspace_id: WorkspaceId,
        target: &mut SnapshotTarget<'_>,
        sources: &SnapshotSources<'_>,
        cx: &App,
    ) {
        let Some(region) = sources.tabs.region(workspace_id) else { return };
        for tab in region.tabs() {
            self.snapshot_tab(tab, target, sources, cx);
        }
        target.layout.set_active_tab(
            workspace_id,
            region
                .active_session()
                .and_then(|tab| region.tabs().iter().position(|entry| entry.session_id == tab))
                .unwrap_or(0),
        );
    }

    /// Copy one tab's pane tree into the scratch layout as its own saved tab.
    fn snapshot_tab(
        &self,
        entry: &TabEntry,
        target: &mut SnapshotTarget<'_>,
        sources: &SnapshotSources<'_>,
        cx: &App,
    ) {
        let (workspace_id, tab) = (entry.workspace_id, entry.session_id);
        let SnapshotTarget { layout, panes } = target;
        let Some(tree) = self.trees.get(&tab) else { return };
        let Some(wire) = pane_node_to_wire(tree.read(cx).tree().root(), &self.sessions) else {
            return;
        };
        let Some(pairs) = layout.add_tab_with_pane_tree(workspace_id, tab, &wire) else { return };
        let focused_session =
            self.focused.get(&tab).and_then(|pane| self.sessions.get(pane)).copied();
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
        if let (Some(pane), Some(saved_tab)) =
            (focused_pane, layout.active_tab_for_workspace_mut(workspace_id))
        {
            saved_tab.focused_pane = pane;
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
        let mut tab_trees = TabTrees::default();
        let mut pending_tab_roots: HashMap<WorkspaceId, VecDeque<SessionId>> = HashMap::new();
        let mut slots = Vec::new();
        for workspace_id in &region_ids {
            let Some(slot) = rebuilt.layout.find_workspace_mut(*workspace_id) else { continue };
            slots.push((*workspace_id, slot.name.clone(), slot.accent_color));
            let active = slot.active_tab;
            for (index, tab) in slot.tabs.iter_mut().enumerate() {
                // The snapshot's session ids are placeholders the replay's own
                // `CreateSession` answers replace; each tab's tree is keyed by
                // one until `sync_tabs` sees that tab in the strip.
                pending_tab_roots.entry(*workspace_id).or_default().push_back(tab.session_id);
                tab_trees.restore_tab(*workspace_id, tab, index == active, cx);
            }
        }

        self.workspace = cx.new(|_| WorkspaceTree::from_tree(&topology));
        self.workspace.update(cx, |tree, ctx| {
            for (workspace_id, name, accent) in slots {
                tree.update_slot(workspace_id, apply_slot_metadata(name, accent), ctx);
            }
            tree.set_focused_workspace(focused_workspace, ctx);
        });
        self.install_tab_trees(tab_trees);
        self.pending_regions.clear();
        self.pending_tab_roots = pending_tab_roots;
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
    /// shell untouched. Every tab of every region comes back with its own pane
    /// split; only the tab each region reported as active is rendered, and its
    /// sessions are the ones returned for the caller to attach.
    pub fn adopt_server_tree(
        &mut self,
        tree: &WorkspaceTreeNode,
        live: &HashSet<SessionId>,
        cx: &mut App,
    ) -> Vec<SessionId> {
        let Some(pruned) = prune_workspace_node(tree, live) else { return Vec::new() };
        let mut leaves = Vec::new();
        wire_leaf_display_tabs(&pruned, &mut leaves);
        let mut tab_trees = TabTrees::default();
        let mut sessions = HashMap::new();
        let mut visible = Vec::new();
        for (workspace_id, tabs, active) in &leaves {
            for (tab, pane_tree) in tabs {
                let shown = *tab == *active;
                let placed = tab_trees.adopt_tab(*workspace_id, *tab, pane_tree, cx);
                sessions.extend(placed.iter().copied().map(|(session, pane)| (pane, session)));
                // Only the tab a region is rendering is attached; the rest stay
                // dormant until the user selects them.
                visible.extend(placed.into_iter().filter(|_| shown).map(|(session, _)| session));
                tab_trees.shown_tabs.extend(shown.then_some((*workspace_id, *tab)));
            }
        }
        if tab_trees.trees.is_empty() {
            return Vec::new();
        }
        self.workspace = cx.new(|_| WorkspaceTree::from_tree(&pruned));
        self.install_tab_trees(tab_trees);
        self.pending_regions.clear();
        self.pending_tab_roots.clear();
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
        self.tab_for_pane(pane, cx).and_then(|tab| self.tab_regions.get(&tab).copied())
    }

    /// The content rect `workspace_id` occupies inside `viewport`, minus a
    /// lower region's tab bar, so pane math and painted panes agree.
    fn region_rect(
        &self,
        workspace_id: WorkspaceId,
        viewport: Rect,
        tab_bar_height: f32,
        cx: &App,
    ) -> Option<Rect> {
        let layout: &WindowLayout = self.workspace.read(cx).layout();
        layout.compute_workspace_rects(viewport).into_iter().find_map(|(id, rect)| {
            (id == workspace_id).then_some(Self::content_rect(
                rect,
                tab_bar_height,
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

/// Build one region's wire payload from its strip tabs and their own pane
/// trees. Pane sessions stay only inside `pane_trees`; they never become tabs.
fn region_tab_payload(
    workspace_id: WorkspaceId,
    tabs: &TabSessions,
    trees: Vec<Option<PaneTreeNode>>,
) -> RegionTabs {
    let Some(region) = tabs.region(workspace_id) else {
        return RegionTabs { session_ids: Vec::new(), pane_trees: Vec::new(), active_tab_index: 0 };
    };
    let session_ids: Vec<SessionId> = region.tabs().iter().map(|tab| tab.session_id).collect();
    let pane_trees = trees
        .into_iter()
        .map(|tree| match tree {
            Some(PaneTreeNode::Split { .. }) => tree,
            Some(PaneTreeNode::Leaf { .. }) | None => None,
        })
        .collect();
    let active_tab_index = region
        .active_session()
        .and_then(|active| session_ids.iter().position(|session_id| *session_id == active))
        .unwrap_or(0);
    RegionTabs { session_ids, pane_trees, active_tab_index }
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

/// One region of a wire tree, lowered for adoption: every tab paired with its
/// own pane split, plus the tab the region reported as active.
type WireRegionTabs = (WorkspaceId, Vec<(SessionId, PaneTreeNode)>, SessionId);

/// Collect every tab's pane tree and its active tab for each leaf, left to
/// right. Inactive trees remain dormant until their strip tab is selected.
fn wire_leaf_display_tabs(node: &WorkspaceTreeNode, out: &mut Vec<WireRegionTabs>) {
    match node {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, pane_trees, active_tab_index } => {
            let tabs = session_ids
                .iter()
                .enumerate()
                .filter_map(|(index, session_id)| {
                    wire_tab_pane_tree(session_ids, pane_trees, index)
                        .map(|tree| (*session_id, tree))
                })
                .collect::<Vec<_>>();
            let index = (*active_tab_index).min(tabs.len().saturating_sub(1));
            let active = tabs.get(index).map(|(active, _)| *active);
            if let Some(active) = active {
                out.push((*workspace_id, tabs, active));
            }
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            wire_leaf_display_tabs(first, out);
            wire_leaf_display_tabs(second, out);
        }
    }
}

/// Sessions that occur only inside a pane tree, paired with their workspace.
/// Reader-side tab reconciliation uses these to keep reconnects from promoting
/// split panes into strip tabs.
///
/// Reads the same lowered leaves adoption does, so a session counts as a pane
/// here exactly when [`wire_leaf_display_tabs`] would hand it to a tab's split
/// rather than to the strip.
#[must_use]
pub fn wire_tree_pane_sessions(node: &WorkspaceTreeNode) -> Vec<(SessionId, WorkspaceId)> {
    let mut leaves = Vec::new();
    wire_leaf_display_tabs(node, &mut leaves);
    let mut panes = Vec::new();
    for (workspace_id, tabs, _) in leaves {
        for (tab, tree) in tabs {
            let mut sessions = Vec::new();
            collect_pane_sessions(&tree, &mut sessions);
            panes.extend(
                sessions
                    .into_iter()
                    .filter(|session_id| *session_id != tab)
                    .map(|session_id| (session_id, workspace_id)),
            );
        }
    }
    panes
}

fn collect_pane_sessions(node: &PaneTreeNode, out: &mut Vec<SessionId>) {
    match node {
        PaneTreeNode::Leaf { session_id } => out.push(*session_id),
        PaneTreeNode::Split { first, second, .. } => {
            collect_pane_sessions(first, out);
            collect_pane_sessions(second, out);
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
            for (index, &session_id) in session_ids.iter().enumerate() {
                let Some(tab) = wire_tab_pane_tree(session_ids, pane_trees, index) else {
                    continue;
                };
                let Some(kept) = prune_pane_node(&tab, live) else { continue };
                if index == *active_tab_index {
                    kept_active = kept_sessions.len();
                }
                // A live tab's identity is its `session_ids` entry, not the
                // split tree's leftmost pane. Replacing an active split tab
                // with `first_session` shifts its pane tree and active index
                // on reconnect (and would make a tear-out re-report a
                // different tree than the server committed).
                kept_sessions.push(if live.contains(&session_id) {
                    session_id
                } else {
                    first_session(&kept)
                });
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

    /// The two per-frame reconcile steps the running view runs, in its order:
    /// give every strip tab its own tree, then hand each tab's still-empty root
    /// pane that tab's own session.
    fn settle(shell: &mut PaneShell, strip: &TabSessions, cx: &mut App) {
        shell.sync_tabs(strip, cx);
        for (_, tab, pane) in shell.empty_unpending_panes(cx) {
            shell.assign_session(pane, tab, cx);
        }
    }

    /// A one-region shell whose single strip tab is on screen.
    fn seeded_shell(cx: &mut App) -> (PaneShell, TabSessions, WorkspaceId, SessionId) {
        let workspace_id = WorkspaceId::new();
        let tab = SessionId::new();
        let mut strip = TabSessions::new();
        strip.insert_active(TabEntry::new(tab, workspace_id, "bash".to_owned()));
        let mut shell = PaneShell::new([0.1, 0.2, 0.3, 1.0], cx);
        shell.adopt_server_workspace(workspace_id, cx);
        settle(&mut shell, &strip, cx);
        (shell, strip, workspace_id, tab)
    }

    /// Split the shown tab and place the session the server answers with, the
    /// way the reader's FIFO pane create and the reconcile pass do together.
    fn split_shown_tab(
        shell: &mut PaneShell,
        strip: &mut TabSessions,
        workspace_id: WorkspaceId,
        cx: &mut App,
    ) -> SessionId {
        let pane = shell
            .split_focused_pane(SplitDirection::Vertical, cx)
            .expect("the shown tab has a focused pane to split");
        let session_id = SessionId::new();
        strip.insert_pane(session_id, workspace_id);
        assert_eq!(shell.take_pending(cx), Some(pane), "the split's own pane claims the answer");
        shell.assign_session(pane, session_id, cx);
        settle(shell, strip, cx);
        session_id
    }

    /// The bug this shell shape exists to fix: `ctrl+shift+\` must add a pane to
    /// the active tab's own tree and nothing at all to the strip.
    // @lat: [[test#GPUI Client Headless Suites#A pane split never adds a strip tab]]
    #[gpui::test]
    fn a_pane_split_never_adds_a_strip_tab(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let (mut shell, mut strip, workspace_id, tab) = seeded_shell(cx);
            assert_eq!(shell.pane_count(cx), 1, "the tab starts as one pane");

            let split = split_shown_tab(&mut shell, &mut strip, workspace_id, cx);

            assert_eq!(strip.len(), 1, "a split leaves the region's tab count unchanged");
            assert_eq!(shell.pane_count(cx), 2, "both panes render");
            let WorkspaceTreeNode::Leaf { session_ids, pane_trees, active_tab_index, .. } =
                shell.wire_tree(&strip, cx)
            else {
                panic!("a one-region window reports one leaf")
            };
            assert_eq!(session_ids, [tab], "the split's session is a pane, never a tab");
            assert_eq!(active_tab_index, 0);
            assert!(
                matches!(pane_trees.first(), Some(Some(PaneTreeNode::Split { .. }))),
                "the active tab's own pane tree carries the split"
            );
            assert_eq!(
                shell.close_tab_sessions(tab, cx),
                vec![tab, split],
                "closing the tab closes every session in its tree"
            );
        });
    }

    /// Tabs own whole trees: selecting one swaps the region's entire layout, and
    /// the split panes inside a tab are never selectable positions of their own.
    // @lat: [[test#GPUI Client Headless Suites#Tab switch swaps the whole per-tab tree]]
    #[gpui::test]
    fn tab_switch_swaps_the_whole_per_tab_tree(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let (mut shell, mut strip, workspace_id, split_tab) = seeded_shell(cx);
            let split = split_shown_tab(&mut shell, &mut strip, workspace_id, cx);

            // A second tab of the same region, opened and therefore shown.
            let plain_tab = SessionId::new();
            strip.insert_active(TabEntry::new(plain_tab, workspace_id, "bash".to_owned()));
            settle(&mut shell, &strip, cx);
            assert_eq!(shell.pane_count(cx), 1, "the split tab's panes went dormant");
            assert_eq!(shell.shown_sessions(cx), [plain_tab].into_iter().collect());

            assert_eq!(
                strip.select(workspace_id, 2),
                None,
                "three live sessions, but the split pane is not a tab position"
            );
            assert_eq!(
                strip.select(workspace_id, 0),
                Some(split_tab),
                "digit selection counts tabs, not split panes"
            );
            let restored = shell.show_tab(workspace_id, split_tab, cx);
            assert_eq!(restored, vec![split_tab, split], "the whole tree comes back at once");
            assert_eq!(shell.pane_count(cx), 2);

            assert_eq!(strip.select(workspace_id, 1), Some(plain_tab));
            assert_eq!(shell.show_tab(workspace_id, plain_tab, cx), vec![plain_tab]);
            assert_eq!(shell.pane_count(cx), 1);
        });
    }

    /// A tab is keyed by the session it opened with, so that session exiting
    /// must not take its splits with it: the surviving pane becomes the tab.
    // @lat: [[test#GPUI Client Headless Suites#A tab re-anchors onto its surviving split]]
    #[gpui::test]
    fn a_tab_re_anchors_onto_its_surviving_split(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let (mut shell, mut strip, workspace_id, tab) = seeded_shell(cx);
            let split = split_shown_tab(&mut shell, &mut strip, workspace_id, cx);

            // The tab's own session exits; the split it made is still running.
            strip.remove(tab);
            let live = [split].into_iter().collect();
            let workspaces_with_tabs = HashSet::new();
            let retired = shell.retain_sessions(&live, &workspaces_with_tabs, cx);

            assert_eq!(retired.promoted_tabs, [split], "the survivor is offered to the strip");
            assert!(retired.closed_regions.is_empty(), "the region still has a live pane");
            assert!(strip.promote_pane(split, "bash".to_owned()));
            settle(&mut shell, &strip, cx);

            assert_eq!(strip.len(), 1, "the region still shows exactly one tab");
            assert_eq!(shell.pane_count(cx), 1);
            assert_eq!(shell.region_shown_session(workspace_id), Some(split));
            let WorkspaceTreeNode::Leaf { session_ids, .. } = shell.wire_tree(&strip, cx) else {
                panic!("a one-region window reports one leaf")
            };
            assert_eq!(session_ids, [split], "the survivor is the tab the server persists");
        });
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
    /// while a stacked region cedes the resolved tab-bar height at its top, and
    /// a region shorter than the bar cannot go negative.
    // @lat: [[test#GPUI Client Headless Suites#Lower regions reserve their tab bar]]
    #[test]
    fn lower_regions_reserve_their_tab_bar() {
        let tab_bar_height = 80.0;
        let top = Rect { x: 0.0, y: 0.0, width: 800.0, height: 300.0 };
        let kept = PaneShell::content_rect(top, tab_bar_height, 0.0, 0.0);
        assert!(
            (kept.y - top.y).abs() < f32::EPSILON
                && (kept.height - top.height).abs() < f32::EPSILON,
            "top-row rect passes through"
        );

        let lower = Rect { x: 0.0, y: 300.0, width: 800.0, height: 300.0 };
        let content = PaneShell::content_rect(lower, tab_bar_height, 0.0, 0.0);
        assert!((content.y - (300.0 + tab_bar_height)).abs() < f32::EPSILON);
        assert!((content.height - (300.0 - tab_bar_height)).abs() < f32::EPSILON);
        assert!((content.x - lower.x).abs() < f32::EPSILON);
        assert!((content.width - lower.width).abs() < f32::EPSILON);

        let sliver = Rect { x: 0.0, y: 300.0, width: 800.0, height: 10.0 };
        let clamped = PaneShell::content_rect(sliver, tab_bar_height, 0.0, 0.0);
        assert!(clamped.height >= 0.0, "a sliver region clamps instead of going negative");
    }

    // @lat: [[test#GPUI CI Run Bar#Band reflows only its workspace region]]
    #[test]
    fn ci_band_reflows_only_its_workspace_region() {
        let top = Rect { x: 400.0, y: 0.0, width: 400.0, height: 600.0 };
        let band = PaneShell::ci_rect(top, 80.0, scribe_client::ci_bar::CI_BAR_HEIGHT)
            .expect("region has room for CI band");
        assert!((band.x - top.x).abs() < f32::EPSILON);
        assert!((band.y - top.y).abs() < f32::EPSILON);
        assert!((band.width - top.width).abs() < f32::EPSILON);
        assert!((band.height - scribe_client::ci_bar::CI_BAR_HEIGHT).abs() < f32::EPSILON);

        let content = PaneShell::content_rect(top, 80.0, scribe_client::ci_bar::CI_BAR_HEIGHT, 0.0);
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
        let trace =
            PaneShell::ci_rect(top, 80.0, expanded).expect("region has room for trace panel");
        assert!((trace.height - expanded).abs() < f32::EPSILON);
        let traced_content = PaneShell::content_rect(top, 80.0, expanded, 0.0);
        assert!((traced_content.y - expanded).abs() < f32::EPSILON);
        assert!((traced_content.height - (top.height - expanded)).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI CI Run Bar#Stacked bands slice one region strip]]
    #[test]
    fn stacked_ci_bands_slice_their_region_strip_in_order() {
        let bar = scribe_client::ci_bar::CI_BAR_HEIGHT;
        let region = Rect { x: 400.0, y: 0.0, width: 400.0, height: 600.0 };
        let strip =
            PaneShell::ci_rect(region, 80.0, 2.0 * bar).expect("region has room for two bands");

        let first = PaneShell::ci_band_rect(strip, 0.0, bar).expect("first band");
        let second = PaneShell::ci_band_rect(strip, bar, bar).expect("second band");
        assert!((first.y - strip.y).abs() < f32::EPSILON);
        assert!((second.y - (strip.y + bar)).abs() < f32::EPSILON);
        assert!((first.height - bar).abs() < f32::EPSILON, "a band keeps its own height");
        assert!((second.height - bar).abs() < f32::EPSILON);
        assert!((first.x - strip.x).abs() < f32::EPSILON, "bands keep the region's columns");
        assert!((second.width - strip.width).abs() < f32::EPSILON);
        assert!(
            (second.y + second.height - (strip.y + strip.height)).abs() < f32::EPSILON,
            "the stack ends exactly where the reserved strip ends"
        );

        let clipped = PaneShell::ci_band_rect(strip, 1.5 * bar, bar).expect("partial band");
        assert!(
            (clipped.height - 0.5 * bar).abs() < f32::EPSILON,
            "a band past the strip's end is clipped, never drawn over the panes"
        );
        assert!(PaneShell::ci_band_rect(strip, 2.0 * bar, bar).is_none(), "no room, no band");
    }

    /// A pinned board takes its strip out of its own region's content, stacking
    /// with a lower region's tab bar and never widening past that region — the
    /// window-wide band it replaced pushed every region's panes down.
    // @lat: [[test#GPUI Client Headless Suites#A pinned board reserves only its own region]]
    #[test]
    fn a_pinned_board_reserves_only_its_own_region() {
        let board = 246.0;
        let tab_bar_height = 80.0;
        let top = Rect { x: 400.0, y: 0.0, width: 400.0, height: 600.0 };
        let with_board = PaneShell::content_rect(top, tab_bar_height, 0.0, board);
        assert!((with_board.y - board).abs() < f32::EPSILON);
        assert!((with_board.height - (600.0 - board)).abs() < f32::EPSILON);
        assert!(
            (with_board.x - top.x).abs() < f32::EPSILON
                && (with_board.width - top.width).abs() < f32::EPSILON,
            "the strip stays inside its own region's columns"
        );

        let lower = Rect { x: 0.0, y: 300.0, width: 800.0, height: 600.0 };
        let stacked = PaneShell::content_rect(lower, tab_bar_height, 0.0, board);
        assert!((stacked.y - (300.0 + tab_bar_height + board)).abs() < f32::EPSILON);

        let shallow =
            PaneShell::content_rect(Rect { height: 100.0, ..top }, tab_bar_height, 0.0, board);
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

    // @lat: [[test#GPUI Workspace Drag]]
    #[test]
    fn pruning_keeps_a_live_active_split_tab_at_its_reported_index() {
        let (workspace_id, first, active, split_right) =
            (WorkspaceId::new(), SessionId::new(), SessionId::new(), SessionId::new());
        let split = PaneTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneTreeNode::Leaf { session_id: first }),
            second: Box::new(PaneTreeNode::Leaf { session_id: split_right }),
        };
        let tree = WorkspaceTreeNode::Leaf {
            workspace_id,
            session_ids: vec![first, active],
            pane_trees: vec![None, Some(split.clone())],
            active_tab_index: 1,
        };
        let live = [first, active, split_right].into_iter().collect();

        let pruned = prune_workspace_node(&tree, &live).expect("live split tab is retained");
        assert_eq!(pruned, tree, "reconnect cannot shift the split onto its first pane tab");
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
        strip.show(second);
        let lone = region_tab_payload(workspace_id, &strip, vec![None, None, None]);
        assert_eq!(lone.session_ids, [first, second, third], "other regions' tabs excluded");
        assert_eq!(lone.active_tab_index, 1);
        assert!(lone.pane_trees.iter().all(Option::is_none), "a lone pane needs no tree");

        // A split stays at its owning tab's index. Its extra pane session is
        // inside the tree, never appended to `session_ids` as another strip tab.
        let split_pane = SessionId::new();
        let split = PaneTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneTreeNode::Leaf { session_id: third }),
            second: Box::new(PaneTreeNode::Leaf { session_id: split_pane }),
        };
        strip.show(third);
        let carried =
            region_tab_payload(workspace_id, &strip, vec![None, None, Some(split.clone())]);
        assert_eq!(carried.session_ids, [first, second, third]);
        assert_eq!(carried.active_tab_index, 2);
        assert_eq!(
            wire_tab_pane_tree(&carried.session_ids, &carried.pane_trees, carried.active_tab_index),
            Some(split),
            "the report round-trips back to the same split"
        );
        let wire = WorkspaceTreeNode::Leaf {
            workspace_id,
            session_ids: carried.session_ids,
            pane_trees: carried.pane_trees,
            active_tab_index: carried.active_tab_index,
        };
        assert_eq!(
            wire_tree_pane_sessions(&wire),
            vec![(split_pane, workspace_id)],
            "reconnect preserves split panes without promoting them to strip tabs"
        );
    }

    // @lat: [[test#GPUI Client Headless Suites#Atomic tab-subtree region transfer]]
    #[gpui::test]
    fn authoritative_tab_subtree_adoption_preserves_tree_order_active_tabs_and_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let (top, lower) = (WorkspaceId::new(), WorkspaceId::new());
            let (moved, pane, top_stays, lower_first, lower_stays) = (
                SessionId::new(),
                SessionId::new(),
                SessionId::new(),
                SessionId::new(),
                SessionId::new(),
            );
            let (before, after) = subtree_adoption_trees(
                (top, lower),
                [moved, pane, top_stays, lower_first, lower_stays],
            );
            let live: HashSet<_> =
                [moved, pane, top_stays, lower_first, lower_stays].into_iter().collect();
            let mut strip = TabSessions::new();
            strip.insert_pane(pane, top);
            strip.reconcile(
                vec![
                    TabEntry::new(moved, top, "moved".to_owned()),
                    TabEntry::new(pane, top, "pane".to_owned()),
                    TabEntry::new(top_stays, top, "top".to_owned()),
                    TabEntry::new(lower_first, lower, "lower-first".to_owned()),
                    TabEntry::new(lower_stays, lower, "lower-stays".to_owned()),
                ],
                Some(pane),
            );
            strip.order_by(&wire_tree_tab_order(&before));
            strip.show(moved);
            strip.show(lower_stays);

            let mut shell = PaneShell::new([0.1, 0.2, 0.3, 1.0], cx);
            shell.adopt_server_tree(&before, &live, cx);
            let (_, focused_pane) = shell.pane_for_session(pane, cx).expect("split pane visible");
            shell.focus_pane(top, focused_pane, cx);
            assert_eq!(shell.focused_session(cx), Some(pane));

            strip.insert_pane(pane, lower);
            strip.reconcile(
                vec![
                    TabEntry::new(top_stays, top, "top".to_owned()),
                    TabEntry::new(lower_first, lower, "lower-first".to_owned()),
                    TabEntry::new(moved, lower, "moved".to_owned()),
                    TabEntry::new(pane, lower, "pane".to_owned()),
                    TabEntry::new(lower_stays, lower, "lower-stays".to_owned()),
                ],
                Some(pane),
            );
            strip.order_by(&wire_tree_tab_order(&after));
            strip.show(top_stays);
            strip.show(moved);

            shell.adopt_server_tree(&after, &live, cx);
            let (_, moved_pane) = shell.pane_for_session(pane, cx).expect("moved pane visible");
            shell.focus_pane(lower, moved_pane, cx);

            assert_eq!(shell.focused_session(cx), Some(pane));
            assert_eq!(shell.region_shown_session(top), Some(top_stays));
            assert_eq!(shell.region_shown_session(lower), Some(moved));
            assert_eq!(
                wire_tree_tab_order(&shell.wire_tree(&strip, cx)),
                [top_stays, lower_first, moved, lower_stays]
            );
            assert_eq!(shell.wire_tree(&strip, cx), after);
        });
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab order spans every region of the tree]]
    #[test]
    fn tab_order_spans_every_region_of_the_tree() {
        let (ws_a, ws_b) = (WorkspaceId::new(), WorkspaceId::new());
        let (a1, a2, b1) = (SessionId::new(), SessionId::new(), SessionId::new());
        let (a_pane, b_pane) = (SessionId::new(), SessionId::new());
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_a,
                session_ids: vec![a1, a2],
                pane_trees: vec![None, Some(pane_split(a2, a_pane))],
                active_tab_index: 1,
            }),
            second: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: ws_b,
                session_ids: vec![b1],
                pane_trees: vec![Some(pane_split(b1, b_pane))],
                active_tab_index: 0,
            }),
        };

        // Background tabs included, regions left to right: this is the order the
        // strip is restored to, so a tab that is not on screen still comes back
        // where the user put it.
        assert_eq!(wire_tree_tab_order(&tree), [a1, a2, b1]);

        // Every region's panes are found too, each under its own workspace, so a
        // reconnect re-files them as panes instead of promoting them to tabs.
        assert_eq!(wire_tree_pane_sessions(&tree), [(a_pane, ws_a), (b_pane, ws_b)]);
    }

    /// A tab whose pane was split once, with `tab` still the first pane.
    fn pane_split(tab: SessionId, pane: SessionId) -> PaneTreeNode {
        PaneTreeNode::Split {
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneTreeNode::Leaf { session_id: tab }),
            second: Box::new(PaneTreeNode::Leaf { session_id: pane }),
        }
    }

    /// The before/after authoritative trees for tab-subtree adoption: the
    /// regression fixture. `moved` — carrying its split with `pane` — leaves
    /// the top region for the middle of the lower region's strip.
    fn subtree_adoption_trees(
        (top, lower): (WorkspaceId, WorkspaceId),
        [moved, pane, top_stays, lower_first, lower_stays]: [SessionId; 5],
    ) -> (WorkspaceTreeNode, WorkspaceTreeNode) {
        let split = pane_split(moved, pane);
        let before = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: top,
                session_ids: vec![moved, top_stays],
                pane_trees: vec![Some(split.clone()), None],
                active_tab_index: 0,
            }),
            second: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: lower,
                session_ids: vec![lower_first, lower_stays],
                pane_trees: vec![None, None],
                active_tab_index: 1,
            }),
        };
        let after = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: top,
                session_ids: vec![top_stays],
                pane_trees: vec![None],
                active_tab_index: 0,
            }),
            second: Box::new(WorkspaceTreeNode::Leaf {
                workspace_id: lower,
                session_ids: vec![lower_first, moved, lower_stays],
                pane_trees: vec![None, Some(split), None],
                active_tab_index: 1,
            }),
        };
        (before, after)
    }
}
