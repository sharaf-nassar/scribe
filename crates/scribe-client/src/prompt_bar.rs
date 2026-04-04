//! GPU-rendered prompt bar for AI terminal panes.
//!
//! Renders a background quad, icon glyphs, and truncated prompt text for
//! the first and latest prompts submitted in a Claude Code or Codex session.

use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::pane::Pane;

/// Background color for the prompt bar: `#151528`.
const BAR_BG: [f32; 4] = [0.082, 0.082, 0.157, 1.0];

/// Text color for the first prompt line: `#7a7a9e`.
const FIRST_TEXT_COLOR: [f32; 4] = [0.478, 0.478, 0.620, 1.0];

/// Text color for the latest prompt line: `#8e8eb5`.
const LATEST_TEXT_COLOR: [f32; 4] = [0.557, 0.557, 0.710, 1.0];

/// Icon color for the first prompt (circle-dot): `#9898bb` at opacity 0.5.
const FIRST_ICON_COLOR: [f32; 4] = [0.298, 0.298, 0.366, 1.0]; // pre-multiplied

/// Icon color for the latest prompt (arrow): `#818cf8` at opacity 0.6.
const LATEST_ICON_COLOR: [f32; 4] = [0.304, 0.329, 0.580, 1.0]; // pre-multiplied

/// Hover highlight text for first prompt: `#7070a0`.
const FIRST_HOVER_TEXT: [f32; 4] = [0.439, 0.439, 0.627, 1.0];

/// Hover highlight text for latest prompt: `#9090bb`.
const LATEST_HOVER_TEXT: [f32; 4] = [0.565, 0.565, 0.733, 1.0];

/// Top padding above the first line.
const TOP_PAD: f32 = 8.0;
/// Bottom padding below the last line (used in `Pane::prompt_bar_height`).
#[allow(dead_code, reason = "documents the prompt bar layout constant used in pane.rs")]
const BOTTOM_PAD: f32 = 6.0;
/// Left padding before the icon.
const LEFT_PAD: f32 = 14.0;
/// Gap between icon and text.
const ICON_TEXT_GAP: f32 = 7.0;

/// Unicode for the circle-dot (origin) icon.
const ICON_FIRST: char = '⊙';
/// Unicode for the right-arrow (latest) icon.
const ICON_LATEST: char = '→';

/// Which prompt bar line the mouse is hovering over, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptBarHover {
    First,
    Latest,
}

/// Render the prompt bar for a pane, returning instances to draw.
///
/// `bar_rect` is the pixel rect for the prompt bar area (between tab bar and
/// terminal content). `cell_size` is `(width, height)` of one glyph cell.
#[allow(
    clippy::too_many_arguments,
    reason = "rendering function needs pane state, geometry, hover state, and glyph resolver"
)]
pub fn render_prompt_bar(
    out: &mut Vec<CellInstance>,
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    hover: Option<PromptBarHover>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    if pane.prompt_count == 0 {
        return;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    // Background quad for the entire bar.
    push_solid_rect(out, bar_rect, BAR_BG);

    // First prompt line.
    if let Some(text) = &pane.first_prompt {
        let y = bar_rect.y + TOP_PAD;
        let is_hovered = hover == Some(PromptBarHover::First);

        if is_hovered {
            let highlight_rect = Rect { x: bar_rect.x, y, width: bar_rect.width, height: cell_h };
            push_solid_rect(out, highlight_rect, [1.0, 1.0, 1.0, 0.025]);
        }

        let icon_color = if is_hovered { FIRST_HOVER_TEXT } else { FIRST_ICON_COLOR };
        let text_color = if is_hovered { FIRST_HOVER_TEXT } else { FIRST_TEXT_COLOR };

        render_prompt_line(
            out,
            ICON_FIRST,
            text,
            bar_rect.x,
            y,
            bar_rect.width,
            icon_color,
            text_color,
            cell_w,
            cell_h,
            resolve_glyph,
        );
    }

    // Latest prompt line (only when 2+ prompts).
    if pane.prompt_count >= 2 {
        if let Some(text) = &pane.latest_prompt {
            let y = bar_rect.y + TOP_PAD + cell_h + 3.0; // 3px gap between lines
            let is_hovered = hover == Some(PromptBarHover::Latest);

            if is_hovered {
                let highlight_rect =
                    Rect { x: bar_rect.x, y, width: bar_rect.width, height: cell_h };
                push_solid_rect(out, highlight_rect, [1.0, 1.0, 1.0, 0.025]);
            }

            let icon_color = if is_hovered { LATEST_HOVER_TEXT } else { LATEST_ICON_COLOR };
            let text_color = if is_hovered { LATEST_HOVER_TEXT } else { LATEST_TEXT_COLOR };

            render_prompt_line(
                out,
                ICON_LATEST,
                text,
                bar_rect.x,
                y,
                bar_rect.width,
                icon_color,
                text_color,
                cell_w,
                cell_h,
                resolve_glyph,
            );
        }
    }
}

/// Render one prompt line: icon + text with truncation.
#[allow(
    clippy::too_many_arguments,
    reason = "line renderer needs all positioning, color, and glyph parameters"
)]
fn render_prompt_line(
    out: &mut Vec<CellInstance>,
    icon: char,
    text: &str,
    bar_x: f32,
    y: f32,
    bar_width: f32,
    icon_color: [f32; 4],
    text_color: [f32; 4],
    cell_w: f32,
    _cell_h: f32,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let mut x = bar_x + LEFT_PAD;
    let max_x = bar_x + bar_width - LEFT_PAD;

    // Icon glyph.
    x = emit_glyph(out, icon, x, y, icon_color, BAR_BG, cell_w, resolve_glyph);
    x += ICON_TEXT_GAP;

    // Text glyphs with truncation.
    let chars: Vec<char> = text.chars().collect();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "pixel count fits in usize; value is non-negative"
    )]
    let available_chars = ((max_x - x) / cell_w) as usize;

    let needs_ellipsis = chars.len() > available_chars;
    let visible_count = if needs_ellipsis {
        available_chars.saturating_sub(1) // leave room for ellipsis
    } else {
        chars.len()
    };

    #[allow(clippy::indexing_slicing, reason = "visible_count <= chars.len() by construction")]
    for &ch in &chars[..visible_count] {
        if x + cell_w > max_x {
            break;
        }
        x = emit_glyph(out, ch, x, y, text_color, BAR_BG, cell_w, resolve_glyph);
    }

    if needs_ellipsis && x + cell_w <= max_x {
        emit_glyph(out, '…', x, y, text_color, BAR_BG, cell_w, resolve_glyph);
    }
}

/// Hit-test: determine which prompt line the mouse is hovering over.
///
/// Returns `None` if the mouse is not within the prompt bar area.
pub fn hit_test_prompt_bar(
    pane: &Pane,
    bar_rect: Rect,
    cell_height: f32,
    mouse_x: f32,
    mouse_y: f32,
) -> Option<PromptBarHover> {
    if pane.prompt_count == 0 {
        return None;
    }

    // Check bounds.
    if mouse_x < bar_rect.x
        || mouse_x > bar_rect.x + bar_rect.width
        || mouse_y < bar_rect.y
        || mouse_y > bar_rect.y + bar_rect.height
    {
        return None;
    }

    let first_y = bar_rect.y + TOP_PAD;
    if mouse_y >= first_y && mouse_y < first_y + cell_height {
        return Some(PromptBarHover::First);
    }

    if pane.prompt_count >= 2 {
        let latest_y = first_y + cell_height + 3.0;
        if mouse_y >= latest_y && mouse_y < latest_y + cell_height {
            return Some(PromptBarHover::Latest);
        }
    }

    None
}

/// Get the full text of the hovered prompt line (for tooltip display).
pub fn hovered_prompt_text(pane: &Pane, hover: PromptBarHover) -> Option<&str> {
    match hover {
        PromptBarHover::First => pane.first_prompt.as_deref(),
        PromptBarHover::Latest => pane.latest_prompt.as_deref(),
    }
}

/// Check whether the given prompt text would be truncated at the given width.
#[allow(clippy::cast_precision_loss, reason = "char count is small, fits in f32")]
pub fn is_prompt_truncated(text: &str, bar_width: f32, cell_w: f32) -> bool {
    let usable = bar_width - LEFT_PAD * 2.0 - cell_w - ICON_TEXT_GAP;
    let text_width = text.chars().count() as f32 * cell_w;
    text_width > usable
}

/// Emit one glyph character at `(x, y)` and return `x + cell_w`.
#[allow(
    clippy::too_many_arguments,
    reason = "glyph emitter needs position, colors, size, and resolver"
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
    out.push(CellInstance {
        pos: [x, y],
        size: [0.0, 0.0],
        uv_min,
        uv_max,
        fg_color,
        bg_color,
        corner_radius: 0.0,
        _pad: 0.0,
    });
    x + cell_w
}

/// Push a solid-color rectangle into `out`.
fn push_solid_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4]) {
    out.push(CellInstance {
        pos: [rect.x, rect.y],
        size: [rect.width, rect.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
        corner_radius: 0.0,
        _pad: 0.0,
    });
}
