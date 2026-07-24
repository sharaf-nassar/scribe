//! Focus-border geometry for panes and workspaces.
//!
//! The focused pane draws a 2px accent border inside its rect; a focused
//! workspace (multi-workspace layouts) draws the same border around the whole
//! workspace region. Both are painted as four solid quads — one per edge — so
//! the corners overlap cleanly without a rounded-join renderer. This module
//! computes those four edge rects; the GPUI paint path fills them with the
//! accent colour in a later bead. Keeping the geometry pure lets the border
//! math be reasoned about without a live window.

use crate::layout::Rect;

/// Accent border width in pixels for a focused pane or workspace.
pub const FOCUS_BORDER_WIDTH: f32 = 2.0;

/// Compute the four edge rects of an accent border inset into `rect`.
///
/// The returned rects are ordered top, bottom, left, right. The left and right
/// strips are shortened by `width` at each end so they do not double-paint the
/// corners already covered by the top and bottom strips. A `rect` smaller than
/// `2 * width` in either axis yields zero-or-clamped strips rather than
/// negative extents.
pub fn border_edges(rect: Rect, width: f32) -> [Rect; 4] {
    let t = width;
    let side_height = (rect.height - t * 2.0).max(0.0);
    [
        // Top strip.
        Rect { x: rect.x, y: rect.y, width: rect.width, height: t },
        // Bottom strip.
        Rect { x: rect.x, y: rect.y + rect.height - t, width: rect.width, height: t },
        // Left strip (between the top and bottom strips).
        Rect { x: rect.x, y: rect.y + t, width: t, height: side_height },
        // Right strip (between the top and bottom strips).
        Rect { x: rect.x + rect.width - t, y: rect.y + t, width: t, height: side_height },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#GPUI Focus Borders#Border edges frame the rect without corner overlap]]
    #[test]
    fn border_edges_frame_rect_without_corner_overlap() {
        let rect = Rect { x: 10.0, y: 20.0, width: 100.0, height: 80.0 };
        let [top, bottom, left, right] = border_edges(rect, FOCUS_BORDER_WIDTH);

        // Top and bottom span the full width at 2px tall.
        assert!((top.width - 100.0).abs() < f32::EPSILON);
        assert!((top.height - 2.0).abs() < f32::EPSILON);
        assert!((top.y - 20.0).abs() < f32::EPSILON);
        assert!((bottom.y - (20.0 + 80.0 - 2.0)).abs() < f32::EPSILON);

        // Left and right are 2px wide and inset vertically by the border width
        // at each end so they never overlap the top/bottom corners.
        assert!((left.width - 2.0).abs() < f32::EPSILON);
        assert!((left.y - 22.0).abs() < f32::EPSILON);
        assert!((left.height - (80.0 - 4.0)).abs() < f32::EPSILON);
        assert!((right.x - (10.0 + 100.0 - 2.0)).abs() < f32::EPSILON);
        assert!((right.height - (80.0 - 4.0)).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Focus Borders#Border side strips clamp on tiny rects]]
    #[test]
    fn border_side_strips_clamp_on_tiny_rects() {
        // Rect shorter than 2 * width: side strips must not go negative.
        let rect = Rect { x: 0.0, y: 0.0, width: 3.0, height: 3.0 };
        let [_, _, left, right] = border_edges(rect, FOCUS_BORDER_WIDTH);
        assert!(left.height >= 0.0);
        assert!(right.height >= 0.0);
    }
}
