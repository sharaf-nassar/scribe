//! Binary split tree for workspace pane layout.
//!
//! The layout tree represents how the terminal window is divided into panes.
//! Each leaf holds a [`PaneId`] and internal nodes describe a horizontal or
//! vertical split with a configurable ratio.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

/// Global monotonic counter ensuring every [`PaneId`] is unique across all
/// layout trees (and therefore across all workspaces).
static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(0);

/// Allocate a globally unique [`PaneId`].
pub fn alloc_pane_id() -> PaneId {
    PaneId(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
}

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

/// Direction for moving pane focus relative to the current pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    /// Move focus to the pane on the left.
    Left,
    /// Move focus to the pane on the right.
    Right,
    /// Move focus to the pane above.
    Up,
    /// Move focus to the pane below.
    Down,
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

impl Rect {
    /// Return `true` if the point `(px, py)` lies inside this rectangle.
    pub fn contains(self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
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

/// Manager for the layout tree that uses globally unique pane IDs.
pub struct LayoutTree {
    root: LayoutNode,
    /// The pane ID assigned to the root leaf when this tree was created.
    initial_pane: PaneId,
}

impl LayoutTree {
    /// Create a new layout tree with a single root pane.
    ///
    /// The root pane receives a globally unique [`PaneId`] allocated from
    /// the module-level atomic counter.
    pub fn new() -> Self {
        let id = alloc_pane_id();
        Self { root: LayoutNode::Leaf(id), initial_pane: id }
    }

    /// Return a reference to the root node.
    pub const fn root(&self) -> &LayoutNode {
        &self.root
    }

    /// Return the pane ID assigned to the root leaf when this tree was
    /// created. Each tree has a unique initial pane ID.
    pub const fn initial_pane_id(&self) -> PaneId {
        self.initial_pane
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
        let new_id = alloc_pane_id();
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

    /// Find the direction of the nearest parent split that contains `pane_id`.
    ///
    /// Returns `None` when the pane is the sole root leaf (no split exists).
    pub fn parent_split_direction(&self, pane_id: PaneId) -> Option<SplitDirection> {
        parent_split_direction_inner(&self.root, pane_id)
    }

    /// Find the split containing `pane_id` and adjust its ratio.
    ///
    /// Returns `true` if the pane was found and the ratio was adjusted.
    #[allow(dead_code, reason = "retained for API completeness; drag now uses set_ratio_for_pane")]
    pub fn adjust_ratio(&mut self, pane_id: PaneId, delta: f32) -> bool {
        adjust_ratio_node(&mut self.root, pane_id, delta)
    }

    /// Find the split containing `pane_id` and set its ratio to an absolute value.
    ///
    /// The ratio is clamped to `[MIN_RATIO, MAX_RATIO]`. Returns `true` if the
    /// pane was found and the ratio was set, `false` otherwise.
    #[allow(dead_code, reason = "public API for drag-resize integration")]
    pub fn set_ratio_for_pane(&mut self, pane_id: PaneId, new_ratio: f32) -> bool {
        set_ratio_node(&mut self.root, pane_id, new_ratio)
    }

    /// Set every split node's ratio to `DEFAULT_RATIO` (0.5).
    #[allow(dead_code, reason = "public API for equalize-all-panes interaction")]
    pub fn equalize_all_ratios(&mut self) {
        equalize_node(&mut self.root);
    }

    /// Find the nearest pane in the given direction from `current`.
    ///
    /// Uses the precomputed `rects` (as returned by [`Self::compute_rects`])
    /// to determine spatial adjacency. Returns `None` when no pane exists in
    /// the requested direction or `current` is not present in `rects`.
    #[allow(
        clippy::unused_self,
        reason = "method semantically belongs to LayoutTree even though rects are pre-computed"
    )]
    pub fn find_pane_in_direction(
        &self,
        current: PaneId,
        direction: FocusDirection,
        rects: &[(PaneId, Rect)],
    ) -> Option<PaneId> {
        let current_rect = rects.iter().find(|(id, _)| *id == current).map(|(_, r)| r)?;
        best_candidate_in_direction(*current_rect, current, direction, rects)
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

/// Recursively find the nearest parent split that contains `target` as
/// a descendant and return its direction.
fn parent_split_direction_inner(node: &LayoutNode, target: PaneId) -> Option<SplitDirection> {
    let LayoutNode::Split { direction, first, second, .. } = node else {
        return None;
    };

    // Check whether the target lives somewhere in either subtree.
    if contains_pane(first, target) || contains_pane(second, target) {
        // Prefer a deeper match first (the nearest ancestor).
        if let Some(dir) = parent_split_direction_inner(first, target) {
            return Some(dir);
        }
        if let Some(dir) = parent_split_direction_inner(second, target) {
            return Some(dir);
        }
        // No deeper split contains the target — this node is the nearest.
        return Some(*direction);
    }

    None
}

/// Check whether `node` contains a leaf with `target`.
fn contains_pane(node: &LayoutNode, target: PaneId) -> bool {
    match node {
        LayoutNode::Leaf(id) => *id == target,
        LayoutNode::Split { first, second, .. } => {
            contains_pane(first, target) || contains_pane(second, target)
        }
    }
}

/// Recursively find the split containing `target` (as a direct child)
/// and adjust its ratio by `delta`, clamping to `[MIN_RATIO, MAX_RATIO]`.
#[allow(dead_code, reason = "called by adjust_ratio which is retained for API completeness")]
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

/// Recursively find the split containing `target` (as a direct child)
/// and set its ratio to `new_ratio`, clamping to `[MIN_RATIO, MAX_RATIO]`.
#[allow(dead_code, reason = "called by set_ratio_for_pane which is part of the public API")]
fn set_ratio_node(node: &mut LayoutNode, target: PaneId, new_ratio: f32) -> bool {
    let LayoutNode::Split { ratio, first, second, .. } = node else {
        return false;
    };

    let first_is_target = matches!(first.as_ref(), LayoutNode::Leaf(id) if *id == target);
    let second_is_target = matches!(second.as_ref(), LayoutNode::Leaf(id) if *id == target);

    if first_is_target || second_is_target {
        *ratio = new_ratio.clamp(MIN_RATIO, MAX_RATIO);
        return true;
    }

    set_ratio_node(first, target, new_ratio) || set_ratio_node(second, target, new_ratio)
}

/// Recursively set every split node's ratio to `DEFAULT_RATIO`.
#[allow(dead_code, reason = "called by equalize_all_ratios which is part of the public API")]
fn equalize_node(node: &mut LayoutNode) {
    let LayoutNode::Split { ratio, first, second, .. } = node else {
        return;
    };

    *ratio = DEFAULT_RATIO;
    equalize_node(first);
    equalize_node(second);
}

// ---------------------------------------------------------------------------
// Directional focus helpers
// ---------------------------------------------------------------------------

/// Return `true` when the closed interval `[a_start, a_end]` overlaps with
/// `[b_start, b_end]` by a non-zero amount.
fn ranges_overlap(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> bool {
    a_start < b_end && b_start < a_end
}

/// Score a candidate pane for directional focus and return the movement-axis
/// distance if the candidate satisfies the spatial constraints.
fn candidate_distance(current: Rect, candidate: Rect, direction: FocusDirection) -> Option<f32> {
    match direction {
        FocusDirection::Right => {
            let past_edge = candidate.x >= current.x + current.width - 1.0;
            let y_overlap = ranges_overlap(
                current.y,
                current.y + current.height,
                candidate.y,
                candidate.y + candidate.height,
            );
            (past_edge && y_overlap).then_some(candidate.x - (current.x + current.width))
        }
        FocusDirection::Left => {
            let past_edge = candidate.x + candidate.width <= current.x + 1.0;
            let y_overlap = ranges_overlap(
                current.y,
                current.y + current.height,
                candidate.y,
                candidate.y + candidate.height,
            );
            (past_edge && y_overlap).then_some(current.x - (candidate.x + candidate.width))
        }
        FocusDirection::Down => {
            let past_edge = candidate.y >= current.y + current.height - 1.0;
            let x_overlap = ranges_overlap(
                current.x,
                current.x + current.width,
                candidate.x,
                candidate.x + candidate.width,
            );
            (past_edge && x_overlap).then_some(candidate.y - (current.y + current.height))
        }
        FocusDirection::Up => {
            let past_edge = candidate.y + candidate.height <= current.y + 1.0;
            let x_overlap = ranges_overlap(
                current.x,
                current.x + current.width,
                candidate.x,
                candidate.x + candidate.width,
            );
            (past_edge && x_overlap).then_some(current.y - (candidate.y + candidate.height))
        }
    }
}

/// Iterate over `rects`, skipping `current_id`, and return the closest pane
/// in the given direction (lowest movement-axis distance).
fn best_candidate_in_direction(
    current_rect: Rect,
    current_id: PaneId,
    direction: FocusDirection,
    rects: &[(PaneId, Rect)],
) -> Option<PaneId> {
    let mut best: Option<(PaneId, f32)> = None;
    for &(id, rect) in rects {
        if id == current_id {
            continue;
        }
        if let Some(dist) = candidate_distance(current_rect, rect, direction) {
            let dominated = best.is_some_and(|(_, d)| d <= dist);
            if !dominated {
                best = Some((id, dist));
            }
        }
    }
    best.map(|(id, _)| id)
}
