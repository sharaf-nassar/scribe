//! Two-level workspace layout: window splits into workspace regions,
//! each workspace region contains tabbed sessions with per-tab pane layouts.
//!
//! The window level divides the viewport into workspace regions via
//! [`WindowLayout`]. Each region holds a [`WorkspaceSlot`] containing
//! tabbed sessions ([`TabState`]). Each tab owns a [`LayoutTree`] for
//! sub-pane splits within that session.

use scribe_common::ids::{SessionId, WorkspaceId};

use crate::layout::{LayoutTree, PaneId, Rect, SplitDirection};

/// Fallback accent colour for new workspaces when no theme is available.
const FALLBACK_ACCENT: [f32; 4] = [0.0, 0.8, 0.7, 1.0];

// ---------------------------------------------------------------------------
// Top-level window layout
// ---------------------------------------------------------------------------

/// Top-level layout: splits the window into workspace regions.
pub struct WindowLayout {
    root: WindowNode,
    focused_workspace: WorkspaceId,
}

/// A node in the window-level split tree.
pub enum WindowNode {
    /// A single workspace region.
    Workspace(WorkspaceSlot),
    /// A split dividing space between two workspace sub-trees.
    #[allow(
        dead_code,
        reason = "multi-workspace splits created via future CreateWorkspace message"
    )]
    Split { direction: SplitDirection, ratio: f32, first: Box<WindowNode>, second: Box<WindowNode> },
}

// ---------------------------------------------------------------------------
// Workspace slot
// ---------------------------------------------------------------------------

/// One workspace region of the window.
pub struct WorkspaceSlot {
    pub workspace_id: WorkspaceId,
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub accent_color: [f32; 4],
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Tab state
// ---------------------------------------------------------------------------

/// One tab (session) within a workspace.
pub struct TabState {
    pub session_id: SessionId,
    pub pane_layout: LayoutTree,
    pub focused_pane: PaneId,
}

// ---------------------------------------------------------------------------
// WindowLayout implementation
// ---------------------------------------------------------------------------

impl WindowLayout {
    /// Create a new window layout containing a single empty workspace.
    ///
    /// `accent` overrides the workspace accent colour; pass `None` to use a
    /// built-in fallback.
    pub fn new(workspace_id: WorkspaceId, accent: Option<[f32; 4]>) -> Self {
        let slot = WorkspaceSlot {
            workspace_id,
            tabs: Vec::new(),
            active_tab: 0,
            accent_color: accent.unwrap_or(FALLBACK_ACCENT),
            name: None,
        };
        Self { root: WindowNode::Workspace(slot), focused_workspace: workspace_id }
    }

    /// Return the focused workspace ID.
    pub const fn focused_workspace_id(&self) -> WorkspaceId {
        self.focused_workspace
    }

    /// Return a reference to the focused workspace slot.
    pub fn focused_workspace(&self) -> Option<&WorkspaceSlot> {
        self.find_workspace(self.focused_workspace)
    }

    /// Return a mutable reference to the focused workspace slot.
    pub fn focused_workspace_mut(&mut self) -> Option<&mut WorkspaceSlot> {
        self.find_workspace_mut(self.focused_workspace)
    }

    /// Shortcut to the focused workspace's active tab.
    pub fn active_tab(&self) -> Option<&TabState> {
        self.focused_workspace().and_then(WorkspaceSlot::active_tab)
    }

    /// Shortcut to the focused workspace's active tab (mutable).
    pub fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.focused_workspace_mut().and_then(WorkspaceSlot::active_tab_mut)
    }

    /// Add a tab to the specified workspace.
    pub fn add_tab(&mut self, workspace_id: WorkspaceId, session_id: SessionId) {
        if let Some(ws) = self.find_workspace_mut(workspace_id) {
            let layout = LayoutTree::new();
            let focused_pane = LayoutTree::initial_pane_id();
            ws.tabs.push(TabState { session_id, pane_layout: layout, focused_pane });
            ws.active_tab = ws.tabs.len().saturating_sub(1);
        }
    }

    /// Remove a tab from the specified workspace.
    pub fn remove_tab(&mut self, workspace_id: WorkspaceId, session_id: SessionId) {
        let Some(ws) = self.find_workspace_mut(workspace_id) else { return };
        let Some(idx) = ws.tabs.iter().position(|t| t.session_id == session_id) else { return };

        ws.tabs.remove(idx);

        // Adjust active_tab so it stays in bounds.
        if ws.tabs.is_empty() {
            ws.active_tab = 0;
        } else if ws.active_tab >= ws.tabs.len() {
            ws.active_tab = ws.tabs.len().saturating_sub(1);
        }
    }

    /// Count the total number of workspace leaves in the tree.
    #[allow(dead_code, reason = "public API for multi-workspace badge rendering")]
    pub fn workspace_count(&self) -> usize {
        count_workspaces(&self.root)
    }

    /// Compute the pixel rect for each workspace leaf, given the full viewport.
    pub fn compute_workspace_rects(&self, viewport: Rect) -> Vec<(WorkspaceId, Rect)> {
        let mut out = Vec::new();
        collect_workspace_rects(&self.root, viewport, &mut out);
        out
    }

    /// Find a workspace slot by ID (immutable).
    pub fn find_workspace(&self, id: WorkspaceId) -> Option<&WorkspaceSlot> {
        find_workspace_in(&self.root, id)
    }

    /// Find a workspace slot by ID (mutable).
    pub fn find_workspace_mut(&mut self, id: WorkspaceId) -> Option<&mut WorkspaceSlot> {
        find_workspace_in_mut(&mut self.root, id)
    }

    /// Find which workspace contains a given session.
    pub fn workspace_for_session(&self, session_id: SessionId) -> Option<WorkspaceId> {
        workspace_for_session_in(&self.root, session_id)
    }
}

// ---------------------------------------------------------------------------
// WorkspaceSlot implementation
// ---------------------------------------------------------------------------

impl WorkspaceSlot {
    /// Return a reference to the active tab, if any.
    pub fn active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab)
    }

    /// Return a mutable reference to the active tab, if any.
    pub fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_tab)
    }
}

// ---------------------------------------------------------------------------
// Recursive helpers
// ---------------------------------------------------------------------------

/// Count workspace leaves in a window node tree.
#[allow(dead_code, reason = "called by workspace_count which is a public API")]
fn count_workspaces(node: &WindowNode) -> usize {
    match node {
        WindowNode::Workspace(_) => 1,
        WindowNode::Split { first, second, .. } => {
            count_workspaces(first) + count_workspaces(second)
        }
    }
}

/// Recursively compute rects for all workspace leaves.
fn collect_workspace_rects(node: &WindowNode, rect: Rect, out: &mut Vec<(WorkspaceId, Rect)>) {
    match node {
        WindowNode::Workspace(slot) => out.push((slot.workspace_id, rect)),
        WindowNode::Split { direction, ratio, first, second } => {
            let (r1, r2) = split_rect(rect, *direction, *ratio);
            collect_workspace_rects(first, r1, out);
            collect_workspace_rects(second, r2, out);
        }
    }
}

/// Divide a rect into two sub-rects along the given direction.
fn split_rect(rect: Rect, direction: SplitDirection, ratio: f32) -> (Rect, Rect) {
    match direction {
        SplitDirection::Horizontal => {
            let left_w = rect.width * ratio;
            let first = Rect { x: rect.x, y: rect.y, width: left_w, height: rect.height };
            let second = Rect {
                x: rect.x + left_w,
                y: rect.y,
                width: rect.width - left_w,
                height: rect.height,
            };
            (first, second)
        }
        SplitDirection::Vertical => {
            let top_h = rect.height * ratio;
            let first = Rect { x: rect.x, y: rect.y, width: rect.width, height: top_h };
            let second = Rect {
                x: rect.x,
                y: rect.y + top_h,
                width: rect.width,
                height: rect.height - top_h,
            };
            (first, second)
        }
    }
}

/// Recursively find a workspace slot by ID.
fn find_workspace_in(node: &WindowNode, id: WorkspaceId) -> Option<&WorkspaceSlot> {
    match node {
        WindowNode::Workspace(slot) if slot.workspace_id == id => Some(slot),
        WindowNode::Workspace(_) => None,
        WindowNode::Split { first, second, .. } => {
            find_workspace_in(first, id).or_else(|| find_workspace_in(second, id))
        }
    }
}

/// Recursively find a mutable workspace slot by ID.
fn find_workspace_in_mut(node: &mut WindowNode, id: WorkspaceId) -> Option<&mut WorkspaceSlot> {
    match node {
        WindowNode::Workspace(slot) if slot.workspace_id == id => Some(slot),
        WindowNode::Workspace(_) => None,
        WindowNode::Split { first, second, .. } => {
            // Try first subtree, then second.
            if let Some(slot) = find_workspace_in_mut(first, id) {
                return Some(slot);
            }
            find_workspace_in_mut(second, id)
        }
    }
}

/// Recursively find which workspace contains the given session.
fn workspace_for_session_in(node: &WindowNode, session_id: SessionId) -> Option<WorkspaceId> {
    match node {
        WindowNode::Workspace(slot) => {
            if slot.tabs.iter().any(|t| t.session_id == session_id) {
                Some(slot.workspace_id)
            } else {
                None
            }
        }
        WindowNode::Split { first, second, .. } => workspace_for_session_in(first, session_id)
            .or_else(|| workspace_for_session_in(second, session_id)),
    }
}
