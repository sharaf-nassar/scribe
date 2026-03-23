//! Binary split tree for workspace pane layout.
//!
//! The layout tree represents how the terminal window is divided into panes.
//! Each leaf holds a [`PaneId`] and internal nodes describe a horizontal or
//! vertical split with a configurable ratio.

use std::fmt;

/// Unique identifier for a pane within the layout tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u32);

impl PaneId {
    /// Create a `PaneId` from a raw `u32` value.
    #[allow(dead_code, reason = "public API for external pane ID construction")]
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    /// Return the inner `u32` value.
    #[allow(dead_code, reason = "public API for pane ID inspection")]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pane-{}", self.0)
    }
}

/// Direction of a split within the layout tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// Split side-by-side (left | right).
    Horizontal,
    /// Split top-over-bottom (top / bottom).
    Vertical,
}

/// Axis-aligned rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A node in the binary split tree.
#[derive(Debug)]
pub enum LayoutNode {
    /// A terminal pane occupying the full extent of its parent rect.
    Leaf(PaneId),
    /// A split dividing space between two children.
    Split {
        direction: SplitDirection,
        /// Fraction of the parent extent allocated to the first child (0.0..=1.0).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Minimum split ratio to prevent degenerate panes.
const MIN_RATIO: f32 = 0.1;

/// Maximum split ratio to prevent degenerate panes.
const MAX_RATIO: f32 = 0.9;

/// Default ratio for new splits.
const DEFAULT_RATIO: f32 = 0.5;

/// Manager for the layout tree plus a monotonic pane ID counter.
pub struct LayoutTree {
    root: LayoutNode,
    next_id: u32,
}

impl LayoutTree {
    /// Create a new layout tree with a single root pane.
    pub fn new() -> Self {
        Self { root: LayoutNode::Leaf(PaneId(0)), next_id: 1 }
    }

    /// Return a reference to the root node.
    pub const fn root(&self) -> &LayoutNode {
        &self.root
    }

    /// Return the initial pane ID (always `PaneId(0)`).
    pub const fn initial_pane_id() -> PaneId {
        PaneId(0)
    }

    /// Allocate and return the next unique pane ID.
    fn alloc_id(&mut self) -> PaneId {
        let id = PaneId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Compute pixel rects for every leaf in the tree.
    pub fn compute_rects(&self, viewport: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        collect_rects(&self.root, viewport, &mut out);
        out
    }

    /// Split the pane identified by `pane_id` in the given direction.
    ///
    /// The existing pane becomes the first child and a new pane becomes the
    /// second child. Returns the new pane's ID, or `None` if the pane was
    /// not found.
    pub fn split_pane(&mut self, pane_id: PaneId, direction: SplitDirection) -> Option<PaneId> {
        let new_id = self.alloc_id();
        if split_node(&mut self.root, pane_id, direction, new_id) { Some(new_id) } else { None }
    }

    /// Remove a pane from the tree, promoting its sibling.
    ///
    /// Returns `true` if the pane was found and removed.
    /// If the pane is the sole root leaf, it is *not* removed.
    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        close_node(&mut self.root, pane_id)
    }

    /// Find a pane in the tree.
    #[allow(dead_code, reason = "public API for pane lookup")]
    pub fn find_pane(&self, pane_id: PaneId) -> Option<&LayoutNode> {
        find_node(&self.root, pane_id)
    }

    /// Cycle to the next pane after `current` in a depth-first order.
    ///
    /// Wraps around to the first pane if `current` is the last.
    pub fn next_pane(&self, current: PaneId) -> PaneId {
        let leaves = collect_leaves(&self.root);
        cycle_pane(&leaves, current)
    }

    /// Collect all leaf pane IDs in depth-first order.
    pub fn all_pane_ids(&self) -> Vec<PaneId> {
        collect_leaves(&self.root)
    }

    /// Find the split containing `pane_id` and adjust its ratio.
    ///
    /// Returns `true` if the pane was found and the ratio was adjusted.
    pub fn adjust_ratio(&mut self, pane_id: PaneId, delta: f32) -> bool {
        adjust_ratio_node(&mut self.root, pane_id, delta)
    }
}

// ---------------------------------------------------------------------------
// Recursive helpers
// ---------------------------------------------------------------------------

/// Recursively compute rects for all leaves.
fn collect_rects(node: &LayoutNode, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        LayoutNode::Leaf(id) => out.push((*id, rect)),
        LayoutNode::Split { direction, ratio, first, second } => {
            let (r1, r2) = split_rect(rect, *direction, *ratio);
            collect_rects(first, r1, out);
            collect_rects(second, r2, out);
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

/// Recursively find the leaf with `target` and replace it with a split.
fn split_node(
    node: &mut LayoutNode,
    target: PaneId,
    direction: SplitDirection,
    new_id: PaneId,
) -> bool {
    match node {
        LayoutNode::Leaf(id) if *id == target => {
            // Replace this leaf with a split containing the old leaf and a new leaf.
            let old_leaf = LayoutNode::Leaf(*id);
            let new_leaf = LayoutNode::Leaf(new_id);
            *node = LayoutNode::Split {
                direction,
                ratio: DEFAULT_RATIO,
                first: Box::new(old_leaf),
                second: Box::new(new_leaf),
            };
            true
        }
        LayoutNode::Leaf(_) => false,
        LayoutNode::Split { first, second, .. } => {
            split_node(first, target, direction, new_id)
                || split_node(second, target, direction, new_id)
        }
    }
}

/// Recursively find and remove a leaf, promoting its sibling.
fn close_node(node: &mut LayoutNode, target: PaneId) -> bool {
    let LayoutNode::Split { first, second, .. } = node else {
        // Cannot close the sole root leaf.
        return false;
    };

    // Check if either child is the target leaf.
    if matches!(first.as_ref(), LayoutNode::Leaf(id) if *id == target) {
        // Promote the second child.
        *node = take_node(second);
        return true;
    }
    if matches!(second.as_ref(), LayoutNode::Leaf(id) if *id == target) {
        // Promote the first child.
        *node = take_node(first);
        return true;
    }

    // Recurse into children.
    close_node(first, target) || close_node(second, target)
}

/// Take ownership of a `LayoutNode` from a `Box`, replacing it with a
/// temporary placeholder that will be immediately overwritten.
fn take_node(boxed: &mut Box<LayoutNode>) -> LayoutNode {
    // Replace the box contents with a dummy leaf; the caller will
    // immediately overwrite the parent node, so the dummy is never used.
    std::mem::replace(boxed.as_mut(), LayoutNode::Leaf(PaneId(u32::MAX)))
}

/// Recursively find a node containing the given pane ID.
#[allow(dead_code, reason = "called by find_pane which is part of the public API")]
fn find_node(node: &LayoutNode, target: PaneId) -> Option<&LayoutNode> {
    match node {
        LayoutNode::Leaf(id) if *id == target => Some(node),
        LayoutNode::Leaf(_) => None,
        LayoutNode::Split { first, second, .. } => {
            find_node(first, target).or_else(|| find_node(second, target))
        }
    }
}

/// Collect all leaf IDs in depth-first order.
fn collect_leaves(node: &LayoutNode) -> Vec<PaneId> {
    let mut out = Vec::new();
    collect_leaves_inner(node, &mut out);
    out
}

fn collect_leaves_inner(node: &LayoutNode, out: &mut Vec<PaneId>) {
    match node {
        LayoutNode::Leaf(id) => out.push(*id),
        LayoutNode::Split { first, second, .. } => {
            collect_leaves_inner(first, out);
            collect_leaves_inner(second, out);
        }
    }
}

/// Return the pane after `current` in `leaves`, wrapping around.
fn cycle_pane(leaves: &[PaneId], current: PaneId) -> PaneId {
    let pos = leaves.iter().position(|id| *id == current);
    pos.map_or_else(
        || leaves.first().copied().unwrap_or(current),
        |idx| {
            let next_idx = (idx + 1) % leaves.len().max(1);
            leaves.get(next_idx).copied().unwrap_or(current)
        },
    )
}

/// Recursively find the split containing `target` (as a direct child)
/// and adjust its ratio by `delta`, clamping to `[MIN_RATIO, MAX_RATIO]`.
fn adjust_ratio_node(node: &mut LayoutNode, target: PaneId, delta: f32) -> bool {
    let LayoutNode::Split { ratio, first, second, .. } = node else {
        return false;
    };

    let first_is_target = matches!(first.as_ref(), LayoutNode::Leaf(id) if *id == target);
    let second_is_target = matches!(second.as_ref(), LayoutNode::Leaf(id) if *id == target);

    if first_is_target || second_is_target {
        *ratio = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
        return true;
    }

    adjust_ratio_node(first, target, delta) || adjust_ratio_node(second, target, delta)
}
