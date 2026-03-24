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
