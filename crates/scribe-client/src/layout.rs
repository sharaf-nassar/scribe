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
    pub const fn from_raw(value: u32) -> Self {
        Self(value)
    }

    /// Return the inner `u32` value.
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

/// Which edges of a pane border the viewport (external edges).
///
/// Internal edges — those adjacent to a sibling pane — should not have
/// content padding applied, preventing visual gaps between panes.
#[derive(Debug, Clone, Copy)]
struct VerticalPaneEdges {
    top: bool,
    bottom: bool,
}

#[derive(Debug, Clone, Copy)]
struct HorizontalPaneEdges {
    right: bool,
    left: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PaneEdges {
    vertical: VerticalPaneEdges,
    horizontal: HorizontalPaneEdges,
}

impl PaneEdges {
    /// All edges are external (single pane, no adjacent siblings).
    pub const fn all_external() -> Self {
        Self {
            vertical: VerticalPaneEdges { top: true, bottom: true },
            horizontal: HorizontalPaneEdges { right: true, left: true },
        }
    }

    /// Return `true` when the top edge borders the viewport.
    pub const fn top(self) -> bool {
        self.vertical.top
    }

    /// Return `true` when the right edge borders the viewport.
    pub const fn right(self) -> bool {
        self.horizontal.right
    }

    /// Return `true` when the bottom edge borders the viewport.
    pub const fn bottom(self) -> bool {
        self.vertical.bottom
    }

    /// Return `true` when the left edge borders the viewport.
    pub const fn left(self) -> bool {
        self.horizontal.left
    }

    const fn without_right(self) -> Self {
        Self {
            vertical: self.vertical,
            horizontal: HorizontalPaneEdges { right: false, left: self.horizontal.left },
        }
    }

    const fn without_left(self) -> Self {
        Self {
            vertical: self.vertical,
            horizontal: HorizontalPaneEdges { right: self.horizontal.right, left: false },
        }
    }

    const fn without_bottom(self) -> Self {
        Self {
            vertical: VerticalPaneEdges { top: self.vertical.top, bottom: false },
            horizontal: self.horizontal,
        }
    }

    const fn without_top(self) -> Self {
        Self {
            vertical: VerticalPaneEdges { top: false, bottom: self.vertical.bottom },
            horizontal: self.horizontal,
        }
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

    /// Construct a layout tree from a pre-built root node.
    ///
    /// `initial_pane` should be the `PaneId` of the first leaf in depth-first
    /// order (used as the default focused pane when no other pane is selected).
    pub fn from_root(root: LayoutNode, initial_pane: PaneId) -> Self {
        Self { root, initial_pane }
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
    pub fn compute_rects(&self, viewport: Rect) -> Vec<(PaneId, Rect, PaneEdges)> {
        let mut out = Vec::new();
        collect_rects(&self.root, viewport, PaneEdges::all_external(), &mut out);
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
    /// Find the split containing `pane_id` and set its ratio to an absolute value.
    ///
    /// The ratio is clamped to `[MIN_RATIO, MAX_RATIO]`. Returns `true` if the
    /// pane was found and the ratio was set, `false` otherwise.
    pub fn set_ratio_for_pane(&mut self, pane_id: PaneId, new_ratio: f32) -> bool {
        set_ratio_node(&mut self.root, pane_id, new_ratio)
    }

    /// Set every split node's ratio to `DEFAULT_RATIO` (0.5).
    pub fn equalize_all_ratios(&mut self) {
        equalize_node(&mut self.root);
    }

    /// Swap the positions of two leaf panes in the tree.
    ///
    /// Uses a three-pass sentinel approach to avoid double-mutable-borrow:
    /// replace `a` with a sentinel, replace `b` with `a`, replace sentinel
    /// with `b`. Returns `true` if both panes were found and swapped.
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> bool {
        swap_panes_in(&mut self.root, a, b)
    }

    /// Find the nearest pane in the given direction from `current`.
    ///
    /// Uses the precomputed `rects` (as returned by [`Self::compute_rects`])
    /// to determine spatial adjacency. If no pane exists in the requested
    /// direction, focus wraps to the opposite edge while preserving
    /// perpendicular-axis overlap. Returns `None` only when no matching pane
    /// exists or `current` is not present in `rects`.
    pub fn find_pane_in_direction(
        &self,
        current: PaneId,
        direction: FocusDirection,
        rects: &[(PaneId, Rect, PaneEdges)],
    ) -> Option<PaneId> {
        if !contains_pane(&self.root, current) {
            return None;
        }
        let current_rect = rects.iter().find(|(id, _, _)| *id == current).map(|(_, r, _)| r)?;
        best_candidate_in_direction(*current_rect, current, direction, rects)
            .or_else(|| wrapped_candidate_in_direction(*current_rect, current, direction, rects))
    }
}

// ---------------------------------------------------------------------------
// Recursive helpers
// ---------------------------------------------------------------------------

/// Recursively compute rects for all leaves.
fn collect_rects(
    node: &LayoutNode,
    rect: Rect,
    edges: PaneEdges,
    out: &mut Vec<(PaneId, Rect, PaneEdges)>,
) {
    match node {
        LayoutNode::Leaf(id) => out.push((*id, rect, edges)),
        LayoutNode::Split { direction, ratio, first, second } => {
            let (r1, r2) = split_rect(rect, *direction, *ratio);
            let (e1, e2) = split_edges(edges, *direction);
            collect_rects(first, r1, e1, out);
            collect_rects(second, r2, e2, out);
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

/// Split edge flags for a parent being divided into two children.
///
/// The child on the split boundary loses its external edge on that side
/// because it now borders a sibling pane, not the viewport.
fn split_edges(edges: PaneEdges, direction: SplitDirection) -> (PaneEdges, PaneEdges) {
    match direction {
        SplitDirection::Horizontal => {
            // Left | Right: first child loses right edge, second loses left.
            (edges.without_right(), edges.without_left())
        }
        SplitDirection::Vertical => {
            // Top / Bottom: first child loses bottom, second loses top.
            (edges.without_bottom(), edges.without_top())
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
/// and set its ratio to `new_ratio`, clamping to `[MIN_RATIO, MAX_RATIO]`.
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

/// Count the number of leaf (pane) nodes in a subtree.
fn count_pane_leaves(node: &LayoutNode) -> u32 {
    match node {
        LayoutNode::Leaf(_) => 1,
        LayoutNode::Split { first, second, .. } => {
            count_pane_leaves(first) + count_pane_leaves(second)
        }
    }
}

/// Recursively set split ratios so every leaf pane gets equal space.
///
/// For a split with `L` leaves on the left and `R` on the right, the ratio
/// is set to `L / (L + R)`.  This ensures each pane gets `1 / total_panes`
/// of the available space regardless of tree shape.
fn equalize_node(node: &mut LayoutNode) {
    let LayoutNode::Split { ratio, first, second, .. } = node else {
        return;
    };

    let left = count_pane_leaves(first);
    let right = count_pane_leaves(second);
    let left_f = f32::from(u16::try_from(left).unwrap_or(u16::MAX));
    let total_f = f32::from(u16::try_from(left + right).unwrap_or(u16::MAX));
    *ratio = left_f / total_f;
    equalize_node(first);
    equalize_node(second);
}

/// Sentinel `PaneId` used internally by `swap_panes_in`.
const SWAP_SENTINEL: PaneId = PaneId(u32::MAX);

/// Swap two leaf panes using a three-pass sentinel technique.
fn swap_panes_in(node: &mut LayoutNode, a: PaneId, b: PaneId) -> bool {
    // Pass 1: replace `a` with the sentinel.
    replace_pane_id(node, a, SWAP_SENTINEL);
    // Pass 2: replace `b` with `a`.
    replace_pane_id(node, b, a);
    // Pass 3: replace sentinel with `b`.
    replace_pane_id(node, SWAP_SENTINEL, b);
    // If the sentinel is gone, both panes were found and swapped.
    !contains_pane(node, SWAP_SENTINEL)
}

/// Recursively replace every leaf whose `PaneId` equals `target` with `replacement`.
fn replace_pane_id(node: &mut LayoutNode, target: PaneId, replacement: PaneId) {
    match node {
        LayoutNode::Leaf(id) if *id == target => *id = replacement,
        LayoutNode::Leaf(_) => {}
        LayoutNode::Split { first, second, .. } => {
            replace_pane_id(first, target, replacement);
            replace_pane_id(second, target, replacement);
        }
    }
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
    rects: &[(PaneId, Rect, PaneEdges)],
) -> Option<PaneId> {
    let mut best: Option<(PaneId, f32)> = None;
    for &(id, rect, _) in rects {
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

fn wrapped_candidate_in_direction(
    current_rect: Rect,
    current_id: PaneId,
    direction: FocusDirection,
    rects: &[(PaneId, Rect, PaneEdges)],
) -> Option<PaneId> {
    let viewport = rect_bounds(rects)?;
    let mut best: Option<(PaneId, f32)> = None;
    for &(id, rect, _) in rects {
        if id == current_id {
            continue;
        }
        let Some(dist) = wrapped_candidate_distance(current_rect, rect, direction, viewport) else {
            continue;
        };
        if best.is_none_or(|(_, best_dist)| dist < best_dist) {
            best = Some((id, dist));
        }
    }
    best.map(|(id, _)| id)
}

fn rect_bounds(rects: &[(PaneId, Rect, PaneEdges)]) -> Option<Rect> {
    let &(_, first, _) = rects.first()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x + first.width;
    let mut max_y = first.y + first.height;

    for &(_, rect, _) in rects.iter().skip(1) {
        min_x = min_x.min(rect.x);
        min_y = min_y.min(rect.y);
        max_x = max_x.max(rect.x + rect.width);
        max_y = max_y.max(rect.y + rect.height);
    }

    Some(Rect { x: min_x, y: min_y, width: max_x - min_x, height: max_y - min_y })
}

fn wrapped_candidate_distance(
    current: Rect,
    candidate: Rect,
    direction: FocusDirection,
    viewport: Rect,
) -> Option<f32> {
    let viewport_right = viewport.x + viewport.width;
    let viewport_bottom = viewport.y + viewport.height;

    match direction {
        FocusDirection::Right => {
            let y_overlap = ranges_overlap(
                current.y,
                current.y + current.height,
                candidate.y,
                candidate.y + candidate.height,
            );
            y_overlap.then_some(candidate.x - viewport.x)
        }
        FocusDirection::Left => {
            let y_overlap = ranges_overlap(
                current.y,
                current.y + current.height,
                candidate.y,
                candidate.y + candidate.height,
            );
            y_overlap.then_some(viewport_right - (candidate.x + candidate.width))
        }
        FocusDirection::Down => {
            let x_overlap = ranges_overlap(
                current.x,
                current.x + current.width,
                candidate.x,
                candidate.x + candidate.width,
            );
            x_overlap.then_some(candidate.y - viewport.y)
        }
        FocusDirection::Up => {
            let x_overlap = ranges_overlap(
                current.x,
                current.x + current.width,
                candidate.x,
                candidate.x + candidate.width,
            );
            x_overlap.then_some(viewport_bottom - (candidate.y + candidate.height))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    type PaneRect = (PaneId, Rect, PaneEdges);
    type ThreePaneRow = (LayoutTree, PaneId, PaneId, PaneId, Vec<PaneRect>);

    fn three_pane_row() -> ThreePaneRow {
        let pane_a = PaneId::from_raw(1);
        let pane_b = PaneId::from_raw(2);
        let pane_c = PaneId::from_raw(3);
        let layout = LayoutTree::from_root(
            LayoutNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 2.0 / 3.0,
                first: Box::new(LayoutNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Leaf(pane_a)),
                    second: Box::new(LayoutNode::Leaf(pane_b)),
                }),
                second: Box::new(LayoutNode::Leaf(pane_c)),
            },
            pane_a,
        );
        let rects = layout.compute_rects(Rect { x: 0.0, y: 0.0, width: 150.0, height: 100.0 });
        (layout, pane_a, pane_b, pane_c, rects)
    }

    #[test]
    fn directional_focus_wraps_right_to_leftmost_overlapping_pane() {
        let (layout, pane_a, _, pane_c, rects) = three_pane_row();
        assert_eq!(
            layout.find_pane_in_direction(pane_c, FocusDirection::Right, &rects),
            Some(pane_a)
        );
    }

    #[test]
    fn directional_focus_prefers_direct_neighbor_before_wrapping() {
        let (layout, _, pane_b, pane_c, rects) = three_pane_row();
        assert_eq!(
            layout.find_pane_in_direction(pane_b, FocusDirection::Right, &rects),
            Some(pane_c)
        );
    }
}
