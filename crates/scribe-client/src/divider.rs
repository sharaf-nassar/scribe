//! Pane divider rendering and drag handling.
//!
//! Dividers are 1px lines between adjacent panes, rendered as solid-colour
//! quads (no glyph atlas — `uv_min == uv_max == [0,0]`).

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

/// Build a 2px accent-coloured border on the focused pane's leading edge.
///
/// For horizontal splits the border appears on the left edge; for vertical
/// splits it appears on the top edge. If `split_direction` is `None` (the
/// pane is the sole root), no border is emitted.
#[allow(
    clippy::cast_precision_loss,
    reason = "step index is a small positive integer fitting in f32"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "border rendering needs rect, direction, color, width, and cell size"
)]
pub fn build_focus_border(
    out: &mut Vec<CellInstance>,
    pane_rect: Rect,
    split_direction: Option<SplitDirection>,
    accent_color: [f32; 4],
    border_width: f32,
    cell_size: (f32, f32),
) {
    let Some(dir) = split_direction else { return };
    let (cell_w, cell_h) = cell_size;

    match dir {
        SplitDirection::Horizontal => {
            // Left edge: vertical stripe, border_width wide.
            let steps = steps_for(pane_rect.height, cell_h);
            for i in 0..steps {
                let y = pane_rect.y + i as f32 * cell_h;
                out.push(solid_quad(pane_rect.x, y, border_width, cell_h, accent_color));
            }
        }
        SplitDirection::Vertical => {
            // Top edge: horizontal stripe, border_width tall.
            let steps = steps_for(pane_rect.width, cell_w);
            for i in 0..steps {
                let x = pane_rect.x + i as f32 * cell_w;
                out.push(solid_quad(x, pane_rect.y, cell_w, border_width, accent_color));
            }
        }
    }
}

/// Build a solid accent border around the entire focused workspace rect.
///
/// Draws four thin quads (top, bottom, left, right edges) so the focused
/// workspace is visually distinct from unfocused siblings.
pub fn build_workspace_focus_border(
    out: &mut Vec<CellInstance>,
    ws_rect: Rect,
    accent_color: [f32; 4],
    border_width: f32,
) {
    let t = border_width;
    // Top edge
    out.push(solid_quad(ws_rect.x, ws_rect.y, ws_rect.width, t, accent_color));
    // Bottom edge
    out.push(solid_quad(ws_rect.x, ws_rect.y + ws_rect.height - t, ws_rect.width, t, accent_color));
    // Left edge (inset by t to avoid corner overlap)
    out.push(solid_quad(ws_rect.x, ws_rect.y + t, t, ws_rect.height - t * 2.0, accent_color));
    // Right edge (inset by t to avoid corner overlap)
    out.push(solid_quad(
        ws_rect.x + ws_rect.width - t,
        ws_rect.y + t,
        t,
        ws_rect.height - t * 2.0,
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

/// Calculate how many cell-sized steps are needed to cover a pixel extent.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "extent / cell_size yields a small positive value fitting in usize"
)]
fn steps_for(extent: f32, cell_size: f32) -> usize {
    if cell_size <= 0.0 { 0 } else { ((extent / cell_size).ceil() as usize).max(1) }
}
