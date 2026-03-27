//! Pane divider rendering and drag handling.
//!
//! Dividers are 1px lines between adjacent panes, rendered as solid-colour
//! quads (no glyph atlas — `uv_min == uv_max == [0,0]`).

use scribe_common::config::ContentPadding;
use scribe_renderer::chrome::solid_quad;
use scribe_renderer::types::CellInstance;

use crate::layout::{LayoutNode, PaneId, Rect, SplitDirection};

/// Divider line thickness in pixels.
const DIVIDER_THICKNESS: f32 = 1.0;

/// Hit-test tolerance: mouse within this many pixels of a divider counts
/// as "on the divider" for drag purposes.
const HIT_TOLERANCE: f32 = 4.0;

/// A divider between two pane groups, positioned in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Divider {
    /// Pixel rect of the divider line.
    pub rect: Rect,
    /// The direction of the split that created this divider.
    pub direction: SplitDirection,
    /// Pane IDs in the first subtree (used for ratio adjustment).
    pub first_pane: PaneId,
    /// Pane ID in the second subtree (used for future resize logic).
    #[allow(dead_code, reason = "will be used for bidirectional divider drag")]
    pub second_pane: PaneId,
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

/// Apply viewport padding insets to divider edges that touch the viewport boundary.
///
/// Horizontal dividers (`SplitDirection::Vertical`) have their left/right edges inset
/// by `padding.left`/`padding.right` when they coincide with the viewport boundary.
/// Vertical dividers (`SplitDirection::Horizontal`) are clipped below the tab bar and
/// have their top/bottom edges inset when they coincide with the viewport boundary.
pub fn apply_viewport_insets(
    dividers: &mut [Divider],
    viewport: Rect,
    padding: &ContentPadding,
    tab_bar_height: f32,
) {
    for d in dividers.iter_mut() {
        match d.direction {
            SplitDirection::Vertical => {
                // Horizontal line: inset left/right edges at viewport boundary.
                let vp_right = viewport.x + viewport.width;
                let d_right = d.rect.x + d.rect.width;
                if (d.rect.x - viewport.x).abs() < 0.5 {
                    d.rect.x += padding.left;
                    d.rect.width -= padding.left;
                }
                if (d_right - vp_right).abs() < 0.5 {
                    d.rect.width -= padding.right;
                }
            }
            SplitDirection::Horizontal => {
                // Vertical line: clip below tab bar and inset top/bottom edges.
                let content_top = viewport.y + tab_bar_height;
                let vp_bottom = viewport.y + viewport.height;
                let d_bottom = d.rect.y + d.rect.height;
                // Clip top below tab bar.
                if d.rect.y < content_top {
                    let clip = content_top - d.rect.y;
                    d.rect.y = content_top;
                    d.rect.height = (d.rect.height - clip).max(0.0);
                }
                // Inset top edge if at content boundary.
                if (d.rect.y - content_top).abs() < 0.5 {
                    d.rect.y += padding.top;
                    d.rect.height = (d.rect.height - padding.top).max(0.0);
                }
                // Inset bottom edge if at viewport boundary.
                if (d_bottom - vp_bottom).abs() < 0.5 {
                    d.rect.height = (d.rect.height - padding.bottom).max(0.0);
                }
            }
        }
    }
}

/// Hit-test: check if a mouse position hits any divider.
///
/// Returns the matching `Divider` if found.
pub fn hit_test_divider(dividers: &[Divider], mouse_x: f32, mouse_y: f32) -> Option<&Divider> {
    dividers.iter().find(|d| is_within_divider(d, mouse_x, mouse_y))
}

/// Build cell instances for all dividers.
///
/// Pushes a single solid-colour quad per divider into `out`. Dividers are
/// 1px lines, so the quad covers the exact divider rect. Pushing directly
/// into the caller's `Vec` avoids a per-call heap allocation.
pub fn build_divider_instances(out: &mut Vec<CellInstance>, dividers: &[Divider], color: [f32; 4]) {
    for divider in dividers {
        build_single_divider(out, divider, color);
    }
}

/// Build a solid accent border around all four edges of a rectangle.
///
/// Used for both focused pane borders and focused workspace borders.
pub fn build_rect_border(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    accent_color: [f32; 4],
    border_width: f32,
) {
    let t = border_width;
    out.push(solid_quad(rect.x, rect.y, rect.width, t, accent_color));
    out.push(solid_quad(rect.x, rect.y + rect.height - t, rect.width, t, accent_color));
    out.push(solid_quad(rect.x, rect.y + t, t, rect.height - t * 2.0, accent_color));
    out.push(solid_quad(
        rect.x + rect.width - t,
        rect.y + t,
        t,
        rect.height - t * 2.0,
        accent_color,
    ));
}

/// Create a `DividerDrag` from a divider and its parent viewport.
pub fn start_drag(divider: &Divider, viewport: Rect) -> DividerDrag {
    let (parent_extent, parent_origin) = match divider.direction {
        SplitDirection::Horizontal => (viewport.width, viewport.x),
        SplitDirection::Vertical => (viewport.height, viewport.y),
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
/// `mouse_pos` is the x or y coordinate depending on direction.
pub fn drag_ratio(drag: &DividerDrag, mouse_pos: f32) -> f32 {
    if drag.parent_extent <= 0.0 {
        return 0.5;
    }
    let relative = mouse_pos - drag.parent_origin;
    (relative / drag.parent_extent).clamp(0.1, 0.9)
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
    let divider_rect = divider_rect_between(&r1, &r2, *direction);
    let first_pane = first_leaf_of(first);
    let second_pane = first_leaf_of(second);

    out.push(Divider { rect: divider_rect, direction: *direction, first_pane, second_pane });

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

/// Compute the pixel rect of a divider between two adjacent rects.
fn divider_rect_between(r1: &Rect, _r2: &Rect, direction: SplitDirection) -> Rect {
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

/// Build a solid-colour instance for a single divider using its actual rect.
fn build_single_divider(instances: &mut Vec<CellInstance>, divider: &Divider, color: [f32; 4]) {
    let r = &divider.rect;
    instances.push(solid_quad(r.x, r.y, r.width, r.height, color));
}
