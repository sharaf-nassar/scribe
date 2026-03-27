//! GPU-rendered tooltip overlay.
//!
//! Renders a small dark box with light text above or below an anchor [`Rect`].
//! The tooltip is positioned centered horizontally on the anchor and emits
//! [`CellInstance`] quads into the caller's buffer.

use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Whether the tooltip should appear above or below its anchor rect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TooltipPosition {
    Above,
    Below,
}

/// A hover target that can show a tooltip.
pub struct TooltipAnchor {
    pub text: String,
    pub rect: Rect,
}

/// Render a tooltip above or below `anchor`, emitting into `out`.
///
/// The tooltip has a 1-character left/right padding, a dark background, and
/// light text. The background is drawn first (solid quad), then text glyphs.
#[allow(
    clippy::too_many_arguments,
    reason = "needs output vec, anchor, position, colors, cell size, and glyph resolver"
)]
pub fn render_tooltip(
    out: &mut Vec<CellInstance>,
    text: &str,
    anchor: Rect,
    position: TooltipPosition,
    bg_color: [f32; 4],
    fg_color: [f32; 4],
    border_color: [f32; 4],
    cell_size: (f32, f32),
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 || text.is_empty() {
        return;
    }

    let text_chars: Vec<char> = text.chars().collect();
    // Padding: 1 char on each side.
    let total_cols = text_chars.len() + 2;

    #[allow(clippy::cast_precision_loss, reason = "total_cols is a small value fitting in f32")]
    let tooltip_w = total_cols as f32 * cell_w;
    let tooltip_h = cell_h;

    // Center horizontally on the anchor rect.
    let center_x = anchor.x + anchor.width / 2.0;
    let tooltip_x = center_x - tooltip_w / 2.0;

    let tooltip_y = match position {
        TooltipPosition::Above => anchor.y - tooltip_h,
        TooltipPosition::Below => anchor.y + anchor.height,
    };

    // Border (1px border rendered as a slightly larger background quad).
    let border_rect = Rect {
        x: tooltip_x - 1.0,
        y: tooltip_y - 1.0,
        width: tooltip_w + 2.0,
        height: tooltip_h + 2.0,
    };
    push_solid_rect(out, border_rect, border_color);

    // Background.
    let bg_rect = Rect { x: tooltip_x, y: tooltip_y, width: tooltip_w, height: tooltip_h };
    push_solid_rect(out, bg_rect, bg_color);

    // Text: leading space + chars + trailing space.
    let mut col_x = tooltip_x;
    col_x = emit_glyph(out, ' ', col_x, tooltip_y, fg_color, bg_color, cell_w, resolve_glyph);
    for &ch in &text_chars {
        col_x = emit_glyph(out, ch, col_x, tooltip_y, fg_color, bg_color, cell_w, resolve_glyph);
    }
    emit_glyph(out, ' ', col_x, tooltip_y, fg_color, bg_color, cell_w, resolve_glyph);
}

/// Emit one glyph character at `(x, y)` and return `x + cell_w`.
#[allow(
    clippy::too_many_arguments,
    reason = "emit helper needs all glyph positioning and color parameters"
)]
fn emit_glyph(
    out: &mut Vec<CellInstance>,
    ch: char,
    x: f32,
    y: f32,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
    cell_w: f32,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> f32 {
    let (uv_min, uv_max) = resolve_glyph(ch);
    out.push(CellInstance { pos: [x, y], size: [0.0, 0.0], uv_min, uv_max, fg_color, bg_color });
    x + cell_w
}

/// Push a solid-color rectangle (no glyph) into `out`.
fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
    });
}
