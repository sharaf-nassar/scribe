//! GPU-rendered tooltip overlay.
//!
//! Renders a small dark box with light text above or below an anchor [`Rect`].
//! The tooltip is positioned centered horizontally on the anchor and emits
//! [`CellInstance`] quads into the caller's buffer.

use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

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

pub struct TooltipRenderContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub text: &'a str,
    pub anchor: Rect,
    pub position: TooltipPosition,
    pub bg_color: [f32; 4],
    pub fg_color: [f32; 4],
    pub border_color: [f32; 4],
    pub cell_size: (f32, f32),
    pub viewport_width: f32,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

/// Render a tooltip above or below `anchor`, emitting into `out`.
///
/// The tooltip has a 1-character left/right padding, a dark background, and
/// light text. The background is drawn first (solid quad), then text glyphs.
/// The tooltip is clamped to stay within `viewport_width`.
pub fn render_tooltip(ctx: TooltipRenderContext<'_>) {
    let TooltipRenderContext {
        out,
        text,
        anchor,
        position,
        bg_color,
        fg_color,
        border_color,
        cell_size,
        viewport_width,
        resolve_glyph,
    } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 || text.is_empty() {
        return;
    }

    let text_chars: Vec<char> = text.chars().collect();
    // Padding: 1 char on each side.
    let total_cols = text_chars.len() + 2;

    let tooltip_w = f32::from(u16::try_from(total_cols).unwrap_or(u16::MAX)) * cell_w;
    let tooltip_h = cell_h;

    // Center horizontally on the anchor rect, clamped to stay within the viewport.
    let center_x = anchor.x + anchor.width / 2.0;
    let tooltip_x = (center_x - tooltip_w / 2.0).clamp(0.0, (viewport_width - tooltip_w).max(0.0));

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
    let mut renderer = TooltipRenderer::new(out, fg_color, bg_color, cell_w, resolve_glyph);
    let mut col_x = tooltip_x;
    col_x = renderer.emit_glyph(' ', col_x, tooltip_y);
    for &ch in &text_chars {
        col_x = renderer.emit_glyph(ch, col_x, tooltip_y);
    }
    renderer.emit_glyph(' ', col_x, tooltip_y);
}

struct TooltipRenderer<'a> {
    out: &'a mut Vec<CellInstance>,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
    cell_w: f32,
    resolve_glyph: &'a mut GlyphResolver<'a>,
}

impl<'a> TooltipRenderer<'a> {
    fn new(
        out: &'a mut Vec<CellInstance>,
        fg_color: [f32; 4],
        bg_color: [f32; 4],
        cell_w: f32,
        resolve_glyph: &'a mut GlyphResolver<'a>,
    ) -> Self {
        Self { out, fg_color, bg_color, cell_w, resolve_glyph }
    }

    fn emit_glyph(&mut self, ch: char, x: f32, y: f32) -> f32 {
        let (uv_min, uv_max) = (self.resolve_glyph)(ch);
        self.out.push(CellInstance {
            pos: [x, y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: self.fg_color,
            bg_color: self.bg_color,
            corner_radius: 0.0,
        });
        x + self.cell_w
    }
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
        corner_radius: 0.0,
    });
}
