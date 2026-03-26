//! Two-level workspace layout: window splits into workspace regions,
//! each workspace region contains tabbed sessions with per-tab pane layouts.
//!
//! The window level divides the viewport into workspace regions via
//! [`WindowLayout`]. Each region holds a [`WorkspaceSlot`] containing
//! tabbed sessions ([`TabState`]). Each tab owns a [`LayoutTree`] for
//! sub-pane splits within that session.

use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{LayoutDirection, WorkspaceTreeNode};

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

    /// Reconstruct a `WindowLayout` from a serialised workspace tree.
    ///
    /// Walks the `WorkspaceTreeNode` recursively, creating empty
    /// `WorkspaceSlot` leaves with fallback accent colours.  The caller
    /// is responsible for populating tabs and setting the correct accent
    /// colours via `WorkspaceInfo` messages afterwards.
    ///
    /// Focus is set to the first workspace leaf found in the tree.
    pub fn from_tree(tree: &WorkspaceTreeNode) -> Self {
        let root = node_from_tree(tree);
        let mut ids = Vec::new();
        collect_workspace_ids(&root, &mut ids);
        let focused = ids.into_iter().next().unwrap_or_else(WorkspaceId::new);
        Self { root, focused_workspace: focused }
    }

    /// Serialise the current layout tree to a `WorkspaceTreeNode` for
    /// reporting to the server.
    pub fn to_tree(&self) -> WorkspaceTreeNode {
        node_to_tree(&self.root)
    }

    /// Return all workspace IDs in tree order (left-to-right depth-first).
    pub fn workspace_ids_in_order(&self) -> Vec<WorkspaceId> {
        let mut ids = Vec::new();
        collect_workspace_ids(&self.root, &mut ids);
        ids
    }

    /// Return the focused workspace ID.
    pub const fn focused_workspace_id(&self) -> WorkspaceId {
        self.focused_workspace
    }

    /// Set the focused workspace. No-op if the ID is already focused.
    pub fn set_focused_workspace(&mut self, id: WorkspaceId) {
        self.focused_workspace = id;
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
    ///
    /// Returns the [`PaneId`] of the new tab's initial pane, or `None` if the
    /// workspace was not found.
    pub fn add_tab(&mut self, workspace_id: WorkspaceId, session_id: SessionId) -> Option<PaneId> {
        let ws = self.find_workspace_mut(workspace_id)?;
        let layout = LayoutTree::new();
        let focused_pane = layout.initial_pane_id();
        let pane_id = focused_pane;
        ws.tabs.push(TabState { session_id, pane_layout: layout, focused_pane });
        ws.active_tab = ws.tabs.len().saturating_sub(1);
        Some(pane_id)
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

    /// Replace a tab's session ID (e.g. when the server confirms creation).
    pub fn update_tab_session(&mut self, old_session_id: SessionId, new_session_id: SessionId) {
        update_tab_session_in(&mut self.root, old_session_id, new_session_id);
    }

    /// Return `true` if the specified workspace has no tabs.
    pub fn is_workspace_empty(&self, workspace_id: WorkspaceId) -> bool {
        self.find_workspace(workspace_id).is_some_and(|ws| ws.tabs.is_empty())
    }

    /// Remove an empty workspace from the layout tree.
    ///
    /// The workspace's sibling is promoted in place of the split node. If the
    /// removed workspace was focused, focus moves to the first remaining
    /// workspace. Returns `true` if the workspace was removed.
    ///
    /// Does nothing (returns `false`) if the workspace is the only one in the
    /// tree — there must always be at least one workspace.
    pub fn remove_workspace(&mut self, workspace_id: WorkspaceId) -> bool {
        // Don't remove the last workspace.
        if matches!(self.root, WindowNode::Workspace(_)) {
            return false;
        }

        if !remove_workspace_node(&mut self.root, workspace_id) {
            return false;
        }

        // If we removed the focused workspace, pick a new focus target.
        if self.focused_workspace == workspace_id {
            let mut ids = Vec::new();
            collect_workspace_ids(&self.root, &mut ids);
            if let Some(&first) = ids.first() {
                self.focused_workspace = first;
            }
        }

        true
    }

    /// Count the total number of workspace leaves in the tree.
    #[allow(dead_code, reason = "public API for multi-workspace badge rendering")]
    pub fn workspace_count(&self) -> usize {
        count_workspaces(&self.root)
    }

    /// Cycle focus to the next workspace in tree order.
    ///
    /// Wraps around to the first workspace after the last. Returns `true` if
    /// focus actually changed.
    pub fn cycle_workspace_focus(&mut self) -> bool {
        let mut ids = Vec::new();
        collect_workspace_ids(&self.root, &mut ids);
        let current = ids.iter().position(|id| *id == self.focused_workspace);
        let next = current.map_or(0, |i| (i + 1) % ids.len());
        if let Some(&new_id) = ids.get(next) {
            if new_id != self.focused_workspace {
                self.focused_workspace = new_id;
                return true;
            }
        }
        false
    }

    /// Compute the pixel rect for each workspace leaf, given the full viewport.
    pub fn compute_workspace_rects(&self, viewport: Rect) -> Vec<(WorkspaceId, Rect)> {
        let mut out = Vec::new();
        collect_workspace_rects(&self.root, viewport, &mut out);
        out
    }

    /// Replace the ID of an existing workspace in the tree.
    ///
    /// Updates `focused_workspace` if the old ID was focused.  Returns `true`
    /// if the workspace was found and renamed.
    pub fn set_workspace_id(&mut self, old_id: WorkspaceId, new_id: WorkspaceId) -> bool {
        let Some(ws) = self.find_workspace_mut(old_id) else {
            return false;
        };
        ws.workspace_id = new_id;
        if self.focused_workspace == old_id {
            self.focused_workspace = new_id;
        }
        true
    }

    /// Split the focused workspace region, creating a new workspace with a
    /// specific ID alongside it.
    ///
    /// Unlike [`split_workspace`] this does *not* move focus to the new
    /// workspace, making it suitable for bulk workspace restoration.
    pub fn split_workspace_with_id(
        &mut self,
        direction: SplitDirection,
        accent: Option<[f32; 4]>,
        workspace_id: WorkspaceId,
    ) -> bool {
        let new_slot = WorkspaceSlot {
            workspace_id,
            tabs: Vec::new(),
            active_tab: 0,
            accent_color: accent.unwrap_or(FALLBACK_ACCENT),
            name: None,
        };
        split_workspace_node(&mut self.root, self.focused_workspace, direction, new_slot).is_ok()
    }

    /// Split the focused workspace region, creating a new workspace alongside it.
    ///
    /// Returns the new workspace ID, or `None` if the focused workspace was
    /// not found in the tree.
    pub fn split_workspace(
        &mut self,
        direction: SplitDirection,
        accent: Option<[f32; 4]>,
    ) -> Option<WorkspaceId> {
        let new_id = WorkspaceId::new();
        let new_slot = WorkspaceSlot {
            workspace_id: new_id,
            tabs: Vec::new(),
            active_tab: 0,
            accent_color: accent.unwrap_or(FALLBACK_ACCENT),
            name: None,
        };

        if split_workspace_node(&mut self.root, self.focused_workspace, direction, new_slot).is_ok()
        {
            self.focused_workspace = new_id;
            Some(new_id)
        } else {
            None
        }
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

    /// Update the split direction of the parent split node that contains the
    /// given workspace.
    ///
    /// Returns `true` if the workspace was found inside a split node and the
    /// direction was updated.  Returns `false` if the workspace is the root
    /// (no parent split) or was not found.
    pub fn update_split_direction_for(
        &mut self,
        workspace_id: WorkspaceId,
        direction: SplitDirection,
    ) -> bool {
        update_split_direction_in(&mut self.root, workspace_id, direction)
    }

    /// Set the active tab index for a workspace.
    ///
    /// Returns `false` if the workspace was not found or `index` is out of
    /// bounds. Returns `true` on success.
    pub fn set_active_tab(&mut self, workspace_id: WorkspaceId, index: usize) -> bool {
        let Some(ws) = self.find_workspace_mut(workspace_id) else {
            return false;
        };
        if index >= ws.tabs.len() {
            return false;
        }
        ws.active_tab = index;
        true
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

    /// Return the index of the next tab, wrapping around to 0 after the last.
    ///
    /// Returns 0 when the tab list is empty.
    pub fn next_tab_index(&self) -> usize {
        let len = self.tabs.len().max(1);
        (self.active_tab + 1) % len
    }

    /// Return the index of the previous tab, wrapping to the last tab from 0.
    ///
    /// Returns 0 when the tab list is empty.
    pub fn prev_tab_index(&self) -> usize {
        let len = self.tabs.len().max(1);
        self.active_tab.checked_sub(1).unwrap_or(len.saturating_sub(1))
    }

    /// Return the number of tabs in this workspace.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Move a tab from one position to another within this workspace.
    ///
    /// Adjusts `active_tab` so the currently active tab remains active after
    /// the reorder.  No-op when `from == to` or either index is out of bounds.
    pub fn reorder_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        // Adjust active_tab to follow the moved tab.
        if self.active_tab == from {
            self.active_tab = to;
        } else if from < self.active_tab && to >= self.active_tab {
            self.active_tab = self.active_tab.saturating_sub(1);
        } else if from > self.active_tab && to <= self.active_tab {
            self.active_tab =
                self.active_tab.saturating_add(1).min(self.tabs.len().saturating_sub(1));
        }
    }
}

// ---------------------------------------------------------------------------
// Recursive helpers
// ---------------------------------------------------------------------------

/// Collect all workspace IDs in tree order (left-to-right / top-to-bottom).
fn collect_workspace_ids(node: &WindowNode, out: &mut Vec<WorkspaceId>) {
    match node {
        WindowNode::Workspace(slot) => out.push(slot.workspace_id),
        WindowNode::Split { first, second, .. } => {
            collect_workspace_ids(first, out);
            collect_workspace_ids(second, out);
        }
    }
}

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

/// Replace the workspace leaf matching `target_id` with a split node
/// containing the original workspace and a new workspace slot.
///
/// Returns `Ok(())` if the target was found and split, or
/// `Err(new_slot)` if the target was not found (returning ownership of
/// the unused slot so the caller can try another subtree).
fn split_workspace_node(
    node: &mut WindowNode,
    target_id: WorkspaceId,
    direction: SplitDirection,
    new_slot: WorkspaceSlot,
) -> Result<(), Box<WorkspaceSlot>> {
    match node {
        WindowNode::Workspace(slot) if slot.workspace_id == target_id => {
            // Take ownership of the current node and replace it with a split.
            let old_node = std::mem::replace(
                node,
                // Temporary placeholder; overwritten immediately below.
                WindowNode::Workspace(WorkspaceSlot {
                    workspace_id: target_id,
                    tabs: Vec::new(),
                    active_tab: 0,
                    accent_color: FALLBACK_ACCENT,
                    name: None,
                }),
            );
            *node = WindowNode::Split {
                direction,
                ratio: 0.5,
                first: Box::new(old_node),
                second: Box::new(WindowNode::Workspace(new_slot)),
            };
            Ok(())
        }
        WindowNode::Workspace(_) => Err(Box::new(new_slot)),
        WindowNode::Split { first, second, .. } => {
            let new_slot = match split_workspace_node(first, target_id, direction, new_slot) {
                Ok(()) => return Ok(()),
                Err(slot) => *slot,
            };
            split_workspace_node(second, target_id, direction, new_slot)
        }
    }
}

/// Remove a workspace leaf from the tree by promoting its sibling.
///
/// When a split contains the target as one child, the entire split node is
/// replaced by the other child. Returns `true` if the workspace was found
/// and removed.
fn remove_workspace_node(node: &mut WindowNode, target_id: WorkspaceId) -> bool {
    let WindowNode::Split { first, second, .. } = node else {
        return false;
    };

    // Check if `first` is the target leaf.
    if matches!(first.as_ref(), WindowNode::Workspace(s) if s.workspace_id == target_id) {
        // Promote second child in place of this split.
        let promoted = std::mem::replace(
            second.as_mut(),
            WindowNode::Workspace(WorkspaceSlot {
                workspace_id: target_id,
                tabs: Vec::new(),
                active_tab: 0,
                accent_color: FALLBACK_ACCENT,
                name: None,
            }),
        );
        *node = promoted;
        return true;
    }

    // Check if `second` is the target leaf.
    if matches!(second.as_ref(), WindowNode::Workspace(s) if s.workspace_id == target_id) {
        let promoted = std::mem::replace(
            first.as_mut(),
            WindowNode::Workspace(WorkspaceSlot {
                workspace_id: target_id,
                tabs: Vec::new(),
                active_tab: 0,
                accent_color: FALLBACK_ACCENT,
                name: None,
            }),
        );
        *node = promoted;
        return true;
    }

    // Recurse into children.
    remove_workspace_node(first, target_id) || remove_workspace_node(second, target_id)
}

/// Walk the tree to find the split node whose direct child is the workspace
/// with the given ID, then update that split node's direction.
///
/// Used as a fallback when reconnecting to an old server that does not send
/// a workspace tree.  When the tree is available, directions are already
/// correct and this function is not called.
fn update_split_direction_in(
    node: &mut WindowNode,
    target_id: WorkspaceId,
    new_direction: SplitDirection,
) -> bool {
    let WindowNode::Split { direction, first, second, .. } = node else {
        return false;
    };

    // If either direct child is the target workspace, update *this* split.
    let first_match =
        matches!(first.as_ref(), WindowNode::Workspace(s) if s.workspace_id == target_id);
    let second_match =
        matches!(second.as_ref(), WindowNode::Workspace(s) if s.workspace_id == target_id);

    if first_match || second_match {
        *direction = new_direction;
        return true;
    }

    // Recurse.
    update_split_direction_in(first, target_id, new_direction)
        || update_split_direction_in(second, target_id, new_direction)
}

// ---------------------------------------------------------------------------
// Tree serialisation / deserialisation helpers
// ---------------------------------------------------------------------------

/// Convert a `LayoutDirection` (protocol) to a `SplitDirection` (client).
fn direction_from_protocol(d: LayoutDirection) -> SplitDirection {
    match d {
        LayoutDirection::Horizontal => SplitDirection::Horizontal,
        LayoutDirection::Vertical => SplitDirection::Vertical,
    }
}

/// Convert a `SplitDirection` (client) to a `LayoutDirection` (protocol).
fn direction_to_protocol(d: SplitDirection) -> LayoutDirection {
    match d {
        SplitDirection::Horizontal => LayoutDirection::Horizontal,
        SplitDirection::Vertical => LayoutDirection::Vertical,
    }
}

/// Recursively build a `WindowNode` tree from a `WorkspaceTreeNode`.
fn node_from_tree(tree: &WorkspaceTreeNode) -> WindowNode {
    match tree {
        WorkspaceTreeNode::Leaf { workspace_id } => WindowNode::Workspace(WorkspaceSlot {
            workspace_id: *workspace_id,
            tabs: Vec::new(),
            active_tab: 0,
            accent_color: FALLBACK_ACCENT,
            name: None,
        }),
        WorkspaceTreeNode::Split { direction, ratio, first, second } => WindowNode::Split {
            direction: direction_from_protocol(*direction),
            ratio: *ratio,
            first: Box::new(node_from_tree(first)),
            second: Box::new(node_from_tree(second)),
        },
    }
}

/// Recursively serialise a `WindowNode` tree to a `WorkspaceTreeNode`.
fn node_to_tree(node: &WindowNode) -> WorkspaceTreeNode {
    match node {
        WindowNode::Workspace(slot) => WorkspaceTreeNode::Leaf { workspace_id: slot.workspace_id },
        WindowNode::Split { direction, ratio, first, second } => WorkspaceTreeNode::Split {
            direction: direction_to_protocol(*direction),
            ratio: *ratio,
            first: Box::new(node_to_tree(first)),
            second: Box::new(node_to_tree(second)),
        },
    }
}

/// Recursively find and update a tab's session ID across all workspaces.
fn update_tab_session_in(node: &mut WindowNode, old_id: SessionId, new_id: SessionId) {
    match node {
        WindowNode::Workspace(slot) => {
            for tab in &mut slot.tabs {
                if tab.session_id == old_id {
                    tab.session_id = new_id;
                    return;
                }
            }
        }
        WindowNode::Split { first, second, .. } => {
            update_tab_session_in(first, old_id, new_id);
            update_tab_session_in(second, old_id, new_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Workspace divider support
// ---------------------------------------------------------------------------

/// Divider line thickness in pixels (matches pane divider).
#[allow(dead_code, reason = "used by workspace divider rendering pipeline")]
const WORKSPACE_DIVIDER_THICKNESS: f32 = 1.0;

/// Hit-test tolerance: mouse within this many pixels counts as on the divider.
#[allow(dead_code, reason = "used by workspace divider hit-test pipeline")]
const WORKSPACE_HIT_TOLERANCE: f32 = 4.0;

/// A divider between two workspace regions, positioned in pixel coordinates.
#[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
pub struct WorkspaceDivider {
    /// Pixel rect of the divider line.
    pub rect: Rect,
    /// The direction of the split that created this divider.
    pub direction: SplitDirection,
    /// First workspace leaf ID in the first subtree.
    pub first_workspace: WorkspaceId,
    /// First workspace leaf ID in the second subtree.
    pub second_workspace: WorkspaceId,
    /// The rect of the parent split node that contains this divider.
    pub parent_rect: Rect,
}

/// State for an in-progress workspace divider drag.
#[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
#[derive(Clone, Copy)]
pub struct WorkspaceDividerDrag {
    /// First workspace leaf in the first subtree of the dragged split.
    pub first_workspace: WorkspaceId,
    /// First workspace leaf in the second subtree of the dragged split.
    pub second_workspace: WorkspaceId,
    /// The direction of the split.
    pub direction: SplitDirection,
    /// The total extent (width or height) of the parent area.
    pub parent_extent: f32,
    /// Pixel position of the parent area origin (x or y).
    pub parent_origin: f32,
}

/// Hit-test: check if a mouse position hits any workspace divider.
///
/// Returns a reference to the matching `WorkspaceDivider` if found.
#[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
pub fn hit_test_workspace_divider(
    dividers: &[WorkspaceDivider],
    mouse_x: f32,
    mouse_y: f32,
) -> Option<&WorkspaceDivider> {
    dividers.iter().find(|d| is_within_workspace_divider(d, mouse_x, mouse_y))
}

/// Create a `WorkspaceDividerDrag` from a workspace divider.
#[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
pub fn start_workspace_drag(divider: &WorkspaceDivider) -> WorkspaceDividerDrag {
    let (parent_extent, parent_origin) = match divider.direction {
        SplitDirection::Horizontal => (divider.parent_rect.width, divider.parent_rect.x),
        SplitDirection::Vertical => (divider.parent_rect.height, divider.parent_rect.y),
    };
    WorkspaceDividerDrag {
        first_workspace: divider.first_workspace,
        second_workspace: divider.second_workspace,
        direction: divider.direction,
        parent_extent,
        parent_origin,
    }
}

/// Compute a new split ratio from a workspace drag position.
///
/// `mouse_pos` is the x or y coordinate depending on direction.
#[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
pub fn workspace_drag_ratio(drag: &WorkspaceDividerDrag, mouse_pos: f32) -> f32 {
    if drag.parent_extent <= 0.0 {
        return 0.5;
    }
    let relative = mouse_pos - drag.parent_origin;
    (relative / drag.parent_extent).clamp(0.1, 0.9)
}

impl WindowLayout {
    /// Collect all workspace divider rects given the full viewport.
    #[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
    pub fn collect_workspace_dividers(&self, viewport: Rect) -> Vec<WorkspaceDivider> {
        let mut out = Vec::new();
        collect_workspace_dividers_inner(&self.root, viewport, &mut out);
        out
    }

    /// Find the split node whose first subtree contains `first_ws` and whose
    /// second subtree contains `second_ws`, then update its ratio to
    /// `new_ratio`, clamped to [0.1, 0.9].
    ///
    /// Using both workspace IDs ensures the correct split is found even when
    /// the same leaf appears as the first leaf of nested splits.
    ///
    /// Returns `true` if the split was found and the ratio updated.
    #[allow(dead_code, reason = "public API for workspace drag-resize pipeline")]
    pub fn set_workspace_ratio(
        &mut self,
        first_ws: WorkspaceId,
        second_ws: WorkspaceId,
        new_ratio: f32,
    ) -> bool {
        set_workspace_ratio_in(&mut self.root, first_ws, second_ws, new_ratio)
    }

    /// Set every split node's ratio to 0.5 recursively.
    #[allow(dead_code, reason = "public API for workspace equalize interaction")]
    pub fn equalize_all_workspace_ratios(&mut self) {
        equalize_workspace_node(&mut self.root);
    }
}

/// Recursively collect workspace dividers from the window node tree.
#[allow(dead_code, reason = "called by collect_workspace_dividers public API")]
fn collect_workspace_dividers_inner(
    node: &WindowNode,
    rect: Rect,
    out: &mut Vec<WorkspaceDivider>,
) {
    let WindowNode::Split { direction, ratio, first, second } = node else {
        return;
    };

    let (r1, r2) = split_rect(rect, *direction, *ratio);

    let divider_rect = workspace_divider_rect_between(&r1, *direction);
    let first_workspace = first_leaf_workspace_of(first);
    let second_workspace = first_leaf_workspace_of(second);

    out.push(WorkspaceDivider {
        rect: divider_rect,
        direction: *direction,
        first_workspace,
        second_workspace,
        parent_rect: rect,
    });

    collect_workspace_dividers_inner(first, r1, out);
    collect_workspace_dividers_inner(second, r2, out);
}

/// Compute the first leaf workspace ID in a subtree (depth-first).
#[allow(dead_code, reason = "called by collect_workspace_dividers_inner")]
fn first_leaf_workspace_of(node: &WindowNode) -> WorkspaceId {
    match node {
        WindowNode::Workspace(slot) => slot.workspace_id,
        WindowNode::Split { first, .. } => first_leaf_workspace_of(first),
    }
}

/// Compute the pixel rect of a workspace divider between two adjacent rects.
#[allow(dead_code, reason = "called by collect_workspace_dividers_inner")]
fn workspace_divider_rect_between(r1: &Rect, direction: SplitDirection) -> Rect {
    let half = WORKSPACE_DIVIDER_THICKNESS / 2.0;
    match direction {
        SplitDirection::Horizontal => {
            let x = r1.x + r1.width - half;
            Rect { x, y: r1.y, width: WORKSPACE_DIVIDER_THICKNESS, height: r1.height }
        }
        SplitDirection::Vertical => {
            let y = r1.y + r1.height - half;
            Rect { x: r1.x, y, width: r1.width, height: WORKSPACE_DIVIDER_THICKNESS }
        }
    }
}

/// Check if a mouse position is within hit-test tolerance of a workspace divider.
#[allow(dead_code, reason = "called by hit_test_workspace_divider public API")]
fn is_within_workspace_divider(divider: &WorkspaceDivider, mouse_x: f32, mouse_y: f32) -> bool {
    let r = &divider.rect;
    let expanded = Rect {
        x: r.x - WORKSPACE_HIT_TOLERANCE,
        y: r.y - WORKSPACE_HIT_TOLERANCE,
        width: r.width + WORKSPACE_HIT_TOLERANCE * 2.0,
        height: r.height + WORKSPACE_HIT_TOLERANCE * 2.0,
    };
    mouse_x >= expanded.x
        && mouse_x <= expanded.x + expanded.width
        && mouse_y >= expanded.y
        && mouse_y <= expanded.y + expanded.height
}

/// Find the split whose first subtree contains `first_ws` and whose second
/// subtree contains `second_ws`, then update its ratio.
#[allow(dead_code, reason = "called by set_workspace_ratio public API")]
fn set_workspace_ratio_in(
    node: &mut WindowNode,
    first_ws: WorkspaceId,
    second_ws: WorkspaceId,
    new_ratio: f32,
) -> bool {
    let WindowNode::Split { ratio, first, second, .. } = node else {
        return false;
    };

    if contains_workspace(first, first_ws) && contains_workspace(second, second_ws) {
        *ratio = new_ratio.clamp(0.1, 0.9);
        return true;
    }

    set_workspace_ratio_in(first, first_ws, second_ws, new_ratio)
        || set_workspace_ratio_in(second, first_ws, second_ws, new_ratio)
}

/// Return `true` if the given workspace ID exists anywhere in the subtree.
fn contains_workspace(node: &WindowNode, target: WorkspaceId) -> bool {
    match node {
        WindowNode::Workspace(s) => s.workspace_id == target,
        WindowNode::Split { first, second, .. } => {
            contains_workspace(first, target) || contains_workspace(second, target)
        }
    }
}

/// Count the number of leaf (workspace) nodes in a subtree.
fn count_workspace_leaves(node: &WindowNode) -> u32 {
    match node {
        WindowNode::Workspace(_) => 1,
        WindowNode::Split { first, second, .. } => {
            count_workspace_leaves(first) + count_workspace_leaves(second)
        }
    }
}

/// Recursively set split ratios so every leaf gets equal space.
///
/// For a split with `L` leaves on the left and `R` on the right, the ratio
/// is set to `L / (L + R)`.  This ensures each leaf gets `1 / total_leaves`
/// of the available space regardless of tree shape.
#[allow(dead_code, reason = "called by equalize_all_workspace_ratios public API")]
fn equalize_workspace_node(node: &mut WindowNode) {
    if let WindowNode::Split { ratio, first, second, .. } = node {
        let left = count_workspace_leaves(first);
        let right = count_workspace_leaves(second);
        #[allow(
            clippy::cast_precision_loss,
            reason = "workspace count is tiny, f32 is exact for small integers"
        )]
        {
            *ratio = left as f32 / (left + right) as f32;
        }
        equalize_workspace_node(first);
        equalize_workspace_node(second);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-leaf tree survives a `to_tree` → `from_tree` roundtrip.
    #[test]
    fn roundtrip_single_leaf() {
        let ws_id = WorkspaceId::new();
        let layout = WindowLayout::new(ws_id, None);

        let wire = layout.to_tree();
        let restored = WindowLayout::from_tree(&wire);

        let ids = restored.workspace_ids_in_order();
        assert_eq!(ids, vec![ws_id]);
        assert_eq!(restored.focused_workspace_id(), ws_id);
    }

    /// Two-workspace split preserves direction and workspace order.
    #[test]
    fn roundtrip_two_workspace_split() {
        let ws_a = WorkspaceId::new();
        let mut layout = WindowLayout::new(ws_a, None);

        let ws_b =
            layout.split_workspace(SplitDirection::Horizontal, None).expect("split should succeed");

        let wire = layout.to_tree();
        let restored = WindowLayout::from_tree(&wire);

        let ids = restored.workspace_ids_in_order();
        assert_eq!(ids, vec![ws_a, ws_b]);
    }

    /// Complex three-workspace tree preserves exact topology and ratios.
    ///
    /// Original tree:
    /// ```text
    ///     V(0.5)
    ///    /      \
    ///   A      H(0.5)
    ///          /    \
    ///         B      C
    /// ```
    #[test]
    fn roundtrip_three_workspace_tree() {
        let ws_a = WorkspaceId::new();
        let mut layout = WindowLayout::new(ws_a, None);

        // Split A → creates B as second child (Vertical = top/bottom).
        let ws_b = layout
            .split_workspace(SplitDirection::Vertical, None)
            .expect("first split should succeed");

        // Focus is now on B. Split B → creates C (Horizontal = side-by-side).
        let ws_c = layout
            .split_workspace(SplitDirection::Horizontal, None)
            .expect("second split should succeed");

        let wire = layout.to_tree();
        let restored = WindowLayout::from_tree(&wire);

        // Leaf order must match: A (top), B (bottom-left), C (bottom-right).
        let ids = restored.workspace_ids_in_order();
        assert_eq!(ids, vec![ws_a, ws_b, ws_c]);

        // Verify the tree structure via rects: A gets the full top half,
        // B and C split the bottom half side-by-side.
        let viewport = Rect { x: 0.0, y: 0.0, width: 1000.0, height: 1000.0 };
        let rects = restored.compute_workspace_rects(viewport);

        // A: full width, top half.
        let a_rect = rects.iter().find(|(id, _)| *id == ws_a).map(|(_, r)| *r).unwrap();
        assert!((a_rect.width - 1000.0).abs() < 1.0);
        assert!((a_rect.height - 500.0).abs() < 1.0);

        // B: left half of bottom.
        let b_rect = rects.iter().find(|(id, _)| *id == ws_b).map(|(_, r)| *r).unwrap();
        assert!((b_rect.width - 500.0).abs() < 1.0);
        assert!((b_rect.height - 500.0).abs() < 1.0);

        // C: right half of bottom.
        let c_rect = rects.iter().find(|(id, _)| *id == ws_c).map(|(_, r)| *r).unwrap();
        assert!((c_rect.width - 500.0).abs() < 1.0);
        assert!((c_rect.x - 500.0).abs() < 1.0);
    }

    /// `to_tree` → `from_tree` preserves a non-default split ratio.
    #[test]
    fn roundtrip_preserves_ratio() {
        let ws_a = WorkspaceId::new();
        let tree = WorkspaceTreeNode::Split {
            direction: LayoutDirection::Vertical,
            ratio: 0.3,
            first: Box::new(WorkspaceTreeNode::Leaf { workspace_id: ws_a }),
            second: Box::new(WorkspaceTreeNode::Leaf { workspace_id: WorkspaceId::new() }),
        };

        let layout = WindowLayout::from_tree(&tree);
        let roundtripped = layout.to_tree();

        // Extract ratio from the roundtripped tree.
        match roundtripped {
            WorkspaceTreeNode::Split { ratio, .. } => {
                assert!((ratio - 0.3).abs() < f32::EPSILON);
            }
            WorkspaceTreeNode::Leaf { .. } => panic!("expected Split, got Leaf"),
        }
    }
}
