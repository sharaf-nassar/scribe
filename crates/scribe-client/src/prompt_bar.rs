//! GPU-rendered prompt bar for AI terminal panes.
//!
//! Renders a background quad, icon glyphs, and truncated prompt text for
//! the first and latest prompts submitted in a Claude Code or Codex session.

use scribe_renderer::types::CellInstance;

use crate::layout::Rect;
use crate::pane::Pane;

/// Configurable colors for the prompt bar, derived from the theme
/// with optional user overrides.
#[derive(Clone, Copy)]
pub struct PromptBarColors {
    pub bg: [f32; 4],
    pub first_row_bg: [f32; 4],
    pub text: [f32; 4],
    pub icon_first: [f32; 4],
    pub icon_latest: [f32; 4],
}

/// Hover highlight for prompt text (white at 5% over the row color).
const HOVER_OVERLAY: [f32; 4] = [1.0, 1.0, 1.0, 0.025];

/// Dismiss button × glyph color (muted).
const DISMISS_COLOR: [f32; 4] = [0.45, 0.45, 0.54, 1.0];

/// Dismiss button × glyph color (hovered).
const DISMISS_HOVER_COLOR: [f32; 4] = [0.80, 0.80, 0.87, 1.0];

/// Width of the dismiss (×) button hit zone at the left edge.
pub const DISMISS_BTN_W: f32 = 28.0;

/// Top padding above the first line.
const TOP_PAD: f32 = 8.0;
/// Bottom padding below the last line (used in `Pane::prompt_bar_height`).
#[allow(dead_code, reason = "documents the prompt bar layout constant used in pane.rs")]
const BOTTOM_PAD: f32 = 8.0;
/// Left padding before the icon.
const LEFT_PAD: f32 = 14.0;
/// Gap between icon and text.
const ICON_TEXT_GAP: f32 = 10.0;

/// Unicode for the circle-dot (origin) icon.
const ICON_FIRST: char = '⊙';
/// Unicode for the right-arrow (latest) icon.
const ICON_LATEST: char = '→';
/// Unicode for dismiss button.
const ICON_DISMISS: char = '×';

/// Which prompt bar line the mouse is hovering over, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptBarHover {
    First,
    Latest,
    DismissButton,
}

/// Render the prompt bar for a pane, returning instances to draw.
///
/// `bar_rect` is the pixel rect for the prompt bar area.
/// `cell_size` is `(width, height)` of one glyph cell (possibly scaled for
/// a custom prompt bar font size). `glyph_size` is the per-instance quad
/// override (`[0.0, 0.0]` to use the uniform, or explicit dimensions when
/// the prompt bar font differs from the terminal font).
#[allow(
    clippy::too_many_arguments,
    reason = "rendering function needs pane state, geometry, hover state, and glyph resolver"
)]
pub fn render_prompt_bar(
    out: &mut Vec<CellInstance>,
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    glyph_size: [f32; 2],
    hover: Option<PromptBarHover>,
    colors: &PromptBarColors,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    if pane.prompt_count == 0 {
        return;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    // Effective width for text lines — dismiss button sits on the left.
    let text_bar_width = bar_rect.width - DISMISS_BTN_W;
    let text_x = bar_rect.x + DISMISS_BTN_W;

    // Background quad for the entire bar.
    push_solid_rect(out, bar_rect, colors.bg);

    // First prompt line — darker background for visual separation.
    if let Some(text) = &pane.first_prompt {
        let y = bar_rect.y + TOP_PAD;
        let is_hovered = hover == Some(PromptBarHover::First);

        // Darker first-row background covers from bar top through the first
        // text line (including TOP_PAD) so the color fills the gap above the text.
        let first_row_rect =
            Rect { x: text_x, y: bar_rect.y, width: text_bar_width, height: cell_h + TOP_PAD };
        push_solid_rect(out, first_row_rect, colors.first_row_bg);

        if is_hovered {
            push_solid_rect(out, first_row_rect, HOVER_OVERLAY);
        }

        render_prompt_line(
            out,
            ICON_FIRST,
            text,
            text_x,
            y,
            text_bar_width,
            colors.icon_first,
            colors.text,
            colors.first_row_bg,
            cell_w,
            cell_h,
            glyph_size,
            resolve_glyph,
        );
    }

    // Latest prompt line (only when 2+ prompts).
    if pane.prompt_count >= 2 {
        if let Some(text) = &pane.latest_prompt {
            let y = bar_rect.y + TOP_PAD + cell_h + 3.0; // 3px gap between lines
            let is_hovered = hover == Some(PromptBarHover::Latest);

            if is_hovered {
                let highlight_rect = Rect { x: text_x, y, width: text_bar_width, height: cell_h };
                push_solid_rect(out, highlight_rect, HOVER_OVERLAY);
            }

            render_prompt_line(
                out,
                ICON_LATEST,
                text,
                text_x,
                y,
                text_bar_width,
                colors.icon_latest,
                colors.text,
                colors.bg,
                cell_w,
                cell_h,
                glyph_size,
                resolve_glyph,
            );
        }
    }

    // Dismiss button (×) at the left edge, vertically centred.
    let dismiss_hovered = hover == Some(PromptBarHover::DismissButton);
    let dismiss_fg = if dismiss_hovered { DISMISS_HOVER_COLOR } else { DISMISS_COLOR };
    let dismiss_x = bar_rect.x + DISMISS_BTN_W / 2.0 - cell_w / 2.0;
    let dismiss_y = bar_rect.y + (bar_rect.height - cell_h) / 2.0;
    emit_glyph(
        out,
        ICON_DISMISS,
        dismiss_x,
        dismiss_y,
        dismiss_fg,
        colors.bg,
        cell_w,
        glyph_size,
        resolve_glyph,
    );
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
    bg_color: [f32; 4],
    cell_w: f32,
    _cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let mut x = bar_x + LEFT_PAD;
    let max_x = bar_x + bar_width - LEFT_PAD;

    // Icon glyph.
    x = emit_glyph(out, icon, x, y, icon_color, bg_color, cell_w, glyph_size, resolve_glyph);
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
        x = emit_glyph(out, ch, x, y, text_color, bg_color, cell_w, glyph_size, resolve_glyph);
    }

    if needs_ellipsis && x + cell_w <= max_x {
        emit_glyph(out, '…', x, y, text_color, bg_color, cell_w, glyph_size, resolve_glyph);
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

    // Dismiss button: leftmost DISMISS_BTN_W pixels of the bar.
    if mouse_x <= bar_rect.x + DISMISS_BTN_W {
        return Some(PromptBarHover::DismissButton);
    }

    // First row hit zone includes the top padding area above the text.
    if mouse_y >= bar_rect.y && mouse_y < bar_rect.y + TOP_PAD + cell_height {
        return Some(PromptBarHover::First);
    }

    if pane.prompt_count >= 2 {
        let latest_y = bar_rect.y + TOP_PAD + cell_height + 3.0;
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
        PromptBarHover::DismissButton => None,
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
///
/// `glyph_size` overrides the uniform cell size when non-zero, allowing the
/// prompt bar to render at a different font scale than the terminal grid.
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
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> f32 {
    let (uv_min, uv_max) = resolve_glyph(ch);
    out.push(CellInstance {
        pos: [x, y],
        size: glyph_size,
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
