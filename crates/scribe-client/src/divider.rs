//! Pane divider geometry and drag-resize math.
//!
//! Dividers are the 1px seams between adjacent panes — resize hit zones,
//! not painted lines (the gaps show bare window ground, per the layered
//! chrome mock). This module owns the pure
//! geometry ported from the legacy client: collecting divider rects from the
//! split tree, applying viewport padding insets, hit-testing with a 4px
//! tolerance, and mapping a drag position back to a split ratio. The GPUI
//! paint path (a solid-quad overlay) and the focus-border quads
//! ([`crate::focus_border`]) consume these rects in a later bead; the logic
//! here stays renderer-independent so it can be exercised by `#[gpui::test]`.

use crate::layout::{LayoutNode, PaneId, Rect, SplitDirection};

/// Divider line thickness in pixels.
pub const DIVIDER_THICKNESS: f32 = 1.0;

/// Hit-test tolerance: a mouse within this many pixels of a divider counts as
/// "on the divider" for drag purposes.
pub const HIT_TOLERANCE: f32 = 4.0;

/// Minimum split ratio a drag may produce (mirrors the layout tree clamp).
const MIN_DRAG_RATIO: f32 = 0.1;

/// Maximum split ratio a drag may produce (mirrors the layout tree clamp).
const MAX_DRAG_RATIO: f32 = 0.9;

/// A divider between two pane groups, positioned in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Divider {
    /// Pixel rect of the divider line.
    pub rect: Rect,
    /// The direction of the split that created this divider.
    pub direction: SplitDirection,
    /// Parent split rect whose axis determines this divider's ratio.
    pub parent_rect: Rect,
    /// First leaf pane in the first subtree (used for ratio adjustment).
    pub first_pane: PaneId,
}

/// State for an in-progress divider drag.
#[derive(Debug, Clone, Copy)]
pub struct DividerDrag {
    /// The first pane adjacent to the divider being dragged.
    pub first_pane: PaneId,
    /// The direction of the split.
    pub direction: SplitDirection,
    /// The total extent (width or height) of the parent area.
    pub parent_extent: f32,
    /// Pixel position of the parent area origin (x or y).
    pub parent_origin: f32,
}

/// Collect all divider rects from the layout tree.
pub fn collect_dividers(node: &LayoutNode, viewport: Rect) -> Vec<Divider> {
    let mut out = Vec::new();
    collect_dividers_inner(node, viewport, &mut out);
    out
}

/// Hit-test: return the first divider within [`HIT_TOLERANCE`] of the mouse.
pub fn hit_test_divider(dividers: &[Divider], mouse_x: f32, mouse_y: f32) -> Option<&Divider> {
    dividers.iter().find(|d| is_within_divider(d, mouse_x, mouse_y))
}

/// Create a [`DividerDrag`] from a divider and its parent viewport.
pub fn start_drag(divider: &Divider, _viewport: Rect) -> DividerDrag {
    let (parent_extent, parent_origin) = match divider.direction {
        SplitDirection::Horizontal => (divider.parent_rect.width, divider.parent_rect.x),
        SplitDirection::Vertical => (divider.parent_rect.height, divider.parent_rect.y),
    };

    DividerDrag {
        first_pane: divider.first_pane,
        direction: divider.direction,
        parent_extent,
        parent_origin,
    }
}

/// Compute a new split ratio from a drag position.
///
/// `mouse_pos` is the x or y coordinate depending on direction. The result is
/// clamped to `[0.1, 0.9]` so a drag can never collapse a pane to zero.
pub fn drag_ratio(drag: &DividerDrag, mouse_pos: f32) -> f32 {
    if drag.parent_extent <= 0.0 {
        return 0.5;
    }
    let relative = mouse_pos - drag.parent_origin;
    (relative / drag.parent_extent).clamp(MIN_DRAG_RATIO, MAX_DRAG_RATIO)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Recursively collect dividers from the layout tree.
fn collect_dividers_inner(node: &LayoutNode, rect: Rect, out: &mut Vec<Divider>) {
    let LayoutNode::Split { direction, ratio, first, second } = node else {
        return;
    };

    let (r1, r2) = split_rects(rect, *direction, *ratio);

    // The divider sits between the two sub-rects.
    let divider_rect = divider_rect_between(&r1, *direction);
    let first_pane = first_leaf_of(first);
    out.push(Divider { rect: divider_rect, direction: *direction, parent_rect: rect, first_pane });

    // Recurse into children.
    collect_dividers_inner(first, r1, out);
    collect_dividers_inner(second, r2, out);
}

/// Compute the first leaf pane ID in a subtree (depth-first).
fn first_leaf_of(node: &LayoutNode) -> PaneId {
    match node {
        LayoutNode::Leaf(id) => *id,
        LayoutNode::Split { first, .. } => first_leaf_of(first),
    }
}

/// Divide a rect into two sub-rects along the given direction.
fn split_rects(rect: Rect, direction: SplitDirection, ratio: f32) -> (Rect, Rect) {
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

/// Compute the pixel rect of a divider on the boundary of `r1`.
fn divider_rect_between(r1: &Rect, direction: SplitDirection) -> Rect {
    let half = DIVIDER_THICKNESS / 2.0;
    match direction {
        SplitDirection::Horizontal => {
            // Divider is a vertical line at the boundary of left and right.
            let x = r1.x + r1.width - half;
            Rect { x, y: r1.y, width: DIVIDER_THICKNESS, height: r1.height }
        }
        SplitDirection::Vertical => {
            // Divider is a horizontal line at the boundary of top and bottom.
            let y = r1.y + r1.height - half;
            Rect { x: r1.x, y, width: r1.width, height: DIVIDER_THICKNESS }
        }
    }
}

/// Check if a mouse position is within hit-test tolerance of a divider.
fn is_within_divider(divider: &Divider, mouse_x: f32, mouse_y: f32) -> bool {
    let r = &divider.rect;
    let expanded = Rect {
        x: r.x - HIT_TOLERANCE,
        y: r.y - HIT_TOLERANCE,
        width: r.width + HIT_TOLERANCE * 2.0,
        height: r.height + HIT_TOLERANCE * 2.0,
    };
    mouse_x >= expanded.x
        && mouse_x <= expanded.x + expanded.width
        && mouse_y >= expanded.y
        && mouse_y <= expanded.y + expanded.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::alloc_pane_id;

    fn leaf() -> (LayoutNode, PaneId) {
        let id = alloc_pane_id();
        (LayoutNode::Leaf(id), id)
    }

    fn viewport() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 800.0, height: 600.0 }
    }

    // @lat: [[test#GPUI Pane Dividers#Horizontal split divider is a centered vertical line]]
    #[test]
    fn horizontal_split_divider_is_centered_vertical_line() {
        let (l, lp) = leaf();
        let (r, _) = leaf();
        let node = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(l),
            second: Box::new(r),
        };
        let dividers = collect_dividers(&node, viewport());
        assert_eq!(dividers.len(), 1);
        let d = dividers[0];
        assert_eq!(d.first_pane, lp);
        assert!((d.rect.width - DIVIDER_THICKNESS).abs() < f32::EPSILON);
        // Centered on the 400px boundary: x = 400 - 0.5.
        assert!((d.rect.x - 399.5).abs() < f32::EPSILON);
        assert!((d.rect.height - 600.0).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Pane Dividers#Vertical split divider is a centered horizontal line]]
    #[test]
    fn vertical_split_divider_is_centered_horizontal_line() {
        let (t, tp) = leaf();
        let (b, _) = leaf();
        let node = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.25,
            first: Box::new(t),
            second: Box::new(b),
        };
        let dividers = collect_dividers(&node, viewport());
        assert_eq!(dividers.len(), 1);
        let d = dividers[0];
        assert_eq!(d.first_pane, tp);
        assert!((d.rect.height - DIVIDER_THICKNESS).abs() < f32::EPSILON);
        // Centered on the 150px boundary: y = 150 - 0.5.
        assert!((d.rect.y - 149.5).abs() < f32::EPSILON);
        assert!((d.rect.width - 800.0).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Pane Dividers#Nested splits yield one divider per split node]]
    #[test]
    fn nested_splits_yield_one_divider_per_split_node() {
        let (a, _) = leaf();
        let (b, _) = leaf();
        let (c, _) = leaf();
        let inner = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(b),
            second: Box::new(c),
        };
        let node = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(a),
            second: Box::new(inner),
        };
        assert_eq!(collect_dividers(&node, viewport()).len(), 2);
    }

    // @lat: [[test#GPUI Pane Dividers#Hit test honors 4px tolerance]]
    #[test]
    fn hit_test_honors_four_pixel_tolerance() {
        let (l, lp) = leaf();
        let (r, _) = leaf();
        let node = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(l),
            second: Box::new(r),
        };
        let dividers = collect_dividers(&node, viewport());
        // Divider spans x in [399.5, 400.5]; tolerance widens to [395.5, 404.5].
        assert!(hit_test_divider(&dividers, 400.0, 300.0).is_some());
        // 4px away from the near edge (399.5 - 3.5 = 396.0) still hits.
        assert!(hit_test_divider(&dividers, 396.0, 300.0).is_some());
        // 5px past the near edge (399.5 - 5 = 394.5) misses.
        assert!(hit_test_divider(&dividers, 394.5, 300.0).is_none());
        let hit = hit_test_divider(&dividers, 400.0, 300.0).unwrap();
        assert_eq!(hit.first_pane, lp);
    }

    // @lat: [[test#GPUI Pane Dividers#Drag maps position to clamped ratio]]
    #[test]
    fn drag_maps_position_to_clamped_ratio() {
        let (l, _) = leaf();
        let (r, _) = leaf();
        let node = LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(l),
            second: Box::new(r),
        };
        let dividers = collect_dividers(&node, viewport());
        let drag = start_drag(&dividers[0], viewport());
        assert!((drag.parent_extent - 800.0).abs() < f32::EPSILON);
        // Mid-drag: 200/800 = 0.25.
        assert!((drag_ratio(&drag, 200.0) - 0.25).abs() < f32::EPSILON);
        // Below the clamp floor: pinned to 0.1.
        assert!((drag_ratio(&drag, 10.0) - 0.1).abs() < f32::EPSILON);
        // Above the clamp ceiling: pinned to 0.9.
        assert!((drag_ratio(&drag, 790.0) - 0.9).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Pane Dividers#Drag on degenerate parent extent falls back to half]]
    #[test]
    fn drag_on_degenerate_parent_extent_falls_back_to_half() {
        let drag = DividerDrag {
            first_pane: alloc_pane_id(),
            direction: SplitDirection::Horizontal,
            parent_extent: 0.0,
            parent_origin: 0.0,
        };
        assert!((drag_ratio(&drag, 123.0) - 0.5).abs() < f32::EPSILON);
    }
}
