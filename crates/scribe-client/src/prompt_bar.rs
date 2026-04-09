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
    pub first_row_bg: [f32; 4],
    pub second_row_bg: [f32; 4],
    pub text: [f32; 4],
    pub icon_first: [f32; 4],
    pub icon_latest: [f32; 4],
}

/// Prompt bar now fills the full prompt-bar rect; outer inset is gone.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_INSET_X: f32 = 0.0;
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_INSET_Y: f32 = 0.0;
/// Square prompt-bar geometry.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_RADIUS: f32 = 0.0;
/// Horizontal padding within a prompt row.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_SIDE_PAD: f32 = 14.0;
/// Rows meet directly; only a thin seam remains between them.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_GAP: f32 = 0.0;
const ROW_SEAM_H: f32 = 1.0;
/// Minimum prompt-row height.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_MIN_HEIGHT: f32 = 28.0;
/// Gap between icon and text.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ICON_TEXT_GAP: f32 = 10.0;
/// Count badge height.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const COUNT_BADGE_H: f32 = 18.0;
/// Horizontal padding inside the count badge.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const COUNT_BADGE_PAD_X: f32 = 8.0;
/// Minimum count badge width.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const COUNT_BADGE_MIN_W: f32 = 30.0;

/// Unicode for the circle-dot (origin) icon.
const ICON_FIRST: char = '⊙';
/// Unicode for the right-arrow (latest) icon.
const ICON_LATEST: char = '→';
/// Unicode for the dismiss overlay icon.
const ICON_DISMISS: char = '×';
const DISMISS_OVERLAY_PAD_Y: f32 = 2.0;

/// Which prompt bar line the mouse is hovering over, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptBarHover {
    First,
    Latest,
    DismissButton,
}

/// Shared prompt-bar geometry for later rendering and hit-testing.
#[allow(dead_code, reason = "shared geometry struct consumed by later renderer work")]
#[derive(Clone, Copy)]
pub struct PromptBarLayout {
    pub card_rect: Rect,
    pub first_row_rect: Rect,
    pub latest_row_rect: Option<Rect>,
    pub seam_rect: Option<Rect>,
    pub count_badge_rect: Option<Rect>,
    pub row_content_x: f32,
    pub first_line_width: f32,
    pub latest_line_width: Option<f32>,
}

/// Compute the row height used by the prompt-bar strip layout.
#[allow(dead_code, reason = "shared geometry helper consumed by later renderer work")]
fn prompt_bar_row_height(cell_height: f32) -> f32 {
    (cell_height + 10.0).max(ROW_MIN_HEIGHT)
}

fn dismiss_overlay_rect(layout: &PromptBarLayout, cell_size: (f32, f32)) -> Rect {
    let (_, cell_h) = cell_size;
    let total_height = layout.latest_row_rect.map_or(layout.first_row_rect.height, |latest| {
        latest.y + latest.height - layout.first_row_rect.y
    });
    // Keep the overlay entirely inside the left padding lane: it covers the
    // seam vertically, but its width stops before the icon lane begins.
    let overlay_h = (cell_h + 4.0).max(18.0);
    let overlay_w = ROW_SIDE_PAD;

    Rect {
        x: layout.card_rect.x,
        y: layout.first_row_rect.y + (total_height - overlay_h) * 0.5 + DISMISS_OVERLAY_PAD_Y,
        width: overlay_w,
        height: (overlay_h - DISMISS_OVERLAY_PAD_Y * 2.0).max(1.0),
    }
}

/// Compute the prompt bar height for the current live strip renderer.
#[allow(dead_code, reason = "shared geometry helper consumed by later pane/layout work")]
#[allow(clippy::cast_precision_loss, reason = "small integer counts map cleanly to f32")]
pub fn prompt_bar_height(prompt_count: u32, cell_height: f32) -> f32 {
    if prompt_count == 0 || cell_height <= 0.0 {
        return 0.0;
    }

    let row_height = prompt_bar_row_height(cell_height);
    if prompt_count >= 2 { row_height * 2.0 + ROW_SEAM_H } else { row_height }
}

/// Compute the shared geometry for the prompt bar strip, rows, badge, and truncation widths.
#[allow(dead_code, reason = "shared geometry helper consumed by later renderer work")]
pub fn compute_prompt_bar_layout(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
) -> Option<PromptBarLayout> {
    if pane.prompt_count == 0 {
        return None;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let row_height = prompt_bar_row_height(cell_h);
    let card_rect = bar_rect;
    let first_row_rect =
        Rect { x: card_rect.x, y: card_rect.y, width: card_rect.width, height: row_height };
    let latest_row_rect = (pane.prompt_count >= 2).then_some(Rect {
        x: card_rect.x,
        y: first_row_rect.y + row_height + ROW_SEAM_H,
        width: card_rect.width,
        height: row_height,
    });
    let seam_rect = latest_row_rect.map(|latest| Rect {
        x: card_rect.x,
        y: latest.y - ROW_SEAM_H,
        width: card_rect.width,
        height: ROW_SEAM_H,
    });
    let row_content_x = card_rect.x;
    let count_badge_rect = if pane.prompt_count > 1 {
        let digit_width =
            pane.prompt_count.to_string().chars().fold(0.0, |width, _| width + cell_w);
        let badge_height = COUNT_BADGE_H.max(cell_h + 4.0);
        let badge_width =
            (digit_width + COUNT_BADGE_PAD_X * 2.0).max(COUNT_BADGE_MIN_W).max(badge_height * 1.5);
        Some(Rect {
            x: card_rect.x + card_rect.width - ROW_SIDE_PAD - badge_width,
            y: first_row_rect.y + (first_row_rect.height - badge_height) * 0.5,
            width: badge_width,
            height: badge_height,
        })
    } else {
        None
    };

    let badge_reserved = count_badge_rect.map_or(0.0, |rect| rect.width + COUNT_BADGE_PAD_X);
    let first_line_width = (card_rect.width - badge_reserved).max(1.0);
    let latest_line_width =
        if pane.prompt_count >= 2 { Some(card_rect.width.max(1.0)) } else { None };

    Some(PromptBarLayout {
        card_rect,
        first_row_rect,
        latest_row_rect,
        seam_rect,
        count_badge_rect,
        row_content_x,
        first_line_width,
        latest_line_width,
    })
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
    clippy::too_many_lines,
    reason = "rendering function needs pane state, geometry, hover state, and glyph resolver"
)]
pub fn render_prompt_bar(
    out: &mut Vec<CellInstance>,
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    glyph_size: [f32; 2],
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
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

    let Some(layout) = compute_prompt_bar_layout(pane, bar_rect, cell_size) else {
        return;
    };
    let first_row_bg = colors.first_row_bg;
    let latest_row_bg = colors.second_row_bg;
    let first_row_hover_bg = lift_color(colors.first_row_bg, 0.035);
    let first_row_active_bg = lift_color(colors.first_row_bg, 0.07);
    let latest_row_hover_bg = lift_color(colors.second_row_bg, 0.035);
    let latest_row_active_bg = lift_color(colors.second_row_bg, 0.07);
    let seam_color = with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.5), 0.20);

    if let Some(text) = &pane.first_prompt {
        push_solid_rect(out, layout.first_row_rect, first_row_bg);
        if hover == Some(PromptBarHover::First) {
            push_solid_rect(out, layout.first_row_rect, first_row_hover_bg);
        }
        if active == Some(PromptBarHover::First) {
            push_solid_rect(out, layout.first_row_rect, first_row_active_bg);
        }

        render_prompt_line(
            out,
            ICON_FIRST,
            text,
            layout.row_content_x,
            layout.first_row_rect.y + (layout.first_row_rect.height - cell_h) * 0.5,
            layout.first_line_width,
            colors.icon_first,
            colors.text,
            first_row_bg,
            cell_w,
            cell_h,
            glyph_size,
            resolve_glyph,
        );
    }

    if let (Some(latest_row_rect), Some(text)) = (layout.latest_row_rect, &pane.latest_prompt) {
        push_solid_rect(out, latest_row_rect, latest_row_bg);
        if hover == Some(PromptBarHover::Latest) {
            push_solid_rect(out, latest_row_rect, latest_row_hover_bg);
        }
        if active == Some(PromptBarHover::Latest) {
            push_solid_rect(out, latest_row_rect, latest_row_active_bg);
        }

        render_prompt_line(
            out,
            ICON_LATEST,
            text,
            layout.row_content_x,
            latest_row_rect.y + (latest_row_rect.height - cell_h) * 0.5,
            layout.latest_line_width.unwrap_or(layout.first_line_width),
            colors.icon_latest,
            colors.text,
            latest_row_bg,
            cell_w,
            cell_h,
            glyph_size,
            resolve_glyph,
        );
    }

    if let Some(seam_rect) = layout.seam_rect {
        push_solid_rect(out, seam_rect, seam_color);
    }

    let show_dismiss_overlay = hover.is_some() || active == Some(PromptBarHover::DismissButton);
    if show_dismiss_overlay {
        let dismiss_rect = dismiss_overlay_rect(&layout, (cell_w, cell_h));
        let dismiss_background = if active == Some(PromptBarHover::DismissButton) {
            with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.38), 1.0)
        } else {
            with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.28), 1.0)
        };
        let dismiss_foreground = if active == Some(PromptBarHover::DismissButton) {
            with_alpha(colors.text, 1.0)
        } else {
            with_alpha(colors.text, 0.94)
        };

        push_solid_rect(out, dismiss_rect, dismiss_background);
        let dismiss_x = dismiss_rect.x + (dismiss_rect.width - cell_w) * 0.5;
        let dismiss_y = dismiss_rect.y + (dismiss_rect.height - cell_h) * 0.5;
        emit_glyph(
            out,
            ICON_DISMISS,
            dismiss_x,
            dismiss_y,
            dismiss_foreground,
            dismiss_background,
            cell_w,
            glyph_size,
            resolve_glyph,
        );
    }

    if let Some(badge_rect) = layout.count_badge_rect {
        render_count_badge(
            out,
            badge_rect,
            pane.prompt_count,
            colors,
            cell_w,
            cell_h,
            glyph_size,
            resolve_glyph,
        );
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
    bg_color: [f32; 4],
    cell_w: f32,
    _cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let mut x = bar_x + ROW_SIDE_PAD;
    let max_x = bar_x + bar_width - ROW_SIDE_PAD;

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

/// Render the count badge shown when multiple prompts are available.
#[allow(
    clippy::too_many_arguments,
    reason = "badge renderer needs geometry, colors, size, and glyph resolver"
)]
fn render_count_badge(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    count: u32,
    colors: &PromptBarColors,
    cell_w: f32,
    cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let badge_fill = mix(colors.second_row_bg, colors.text, 0.06);
    let badge_text = with_alpha(colors.text, 0.96);

    push_solid_rect(out, rect, badge_fill);

    let text = count.to_string();
    let text_width = text.chars().fold(0.0, |width, _| width + cell_w);
    let mut x = rect.x + (rect.width - text_width).max(0.0) * 0.5;
    let y = rect.y + (rect.height - cell_h) * 0.5;
    for ch in text.chars() {
        x = emit_glyph(out, ch, x, y, badge_text, badge_fill, cell_w, glyph_size, resolve_glyph);
    }
}

/// Hit-test: determine which prompt line the mouse is hovering over.
///
/// Returns `None` if the mouse is not within the prompt bar area.
pub fn hit_test_prompt_bar(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    mouse_x: f32,
    mouse_y: f32,
) -> Option<PromptBarHover> {
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size)?;
    if !layout.card_rect.contains(mouse_x, mouse_y) {
        return None;
    }

    let dismiss_rect = dismiss_overlay_rect(&layout, cell_size);
    if dismiss_rect.contains(mouse_x, mouse_y) {
        return Some(PromptBarHover::DismissButton);
    }

    if layout.first_row_rect.contains(mouse_x, mouse_y) {
        return Some(PromptBarHover::First);
    }

    if layout
        .latest_row_rect
        .is_some_and(|latest_row_rect| latest_row_rect.contains(mouse_x, mouse_y))
    {
        return Some(PromptBarHover::Latest);
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

/// Get the effective row width used to determine truncation for the hovered prompt line.
pub fn prompt_bar_text_width(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    hover: PromptBarHover,
) -> Option<f32> {
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size)?;
    match hover {
        PromptBarHover::First => Some(layout.first_line_width),
        PromptBarHover::Latest => layout.latest_line_width,
        PromptBarHover::DismissButton => None,
    }
}

/// Check whether the given prompt text would be truncated at the given width.
#[allow(clippy::cast_precision_loss, reason = "char count is small, fits in f32")]
pub fn is_prompt_truncated(text: &str, bar_width: f32, cell_w: f32) -> bool {
    let usable = bar_width - ROW_SIDE_PAD * 2.0 - cell_w - ICON_TEXT_GAP;
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
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }

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

/// Blend `a` toward `b` by `t`.
fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Lift a row color toward white without changing the row's identity.
fn lift_color(color: [f32; 4], amount: f32) -> [f32; 4] {
    mix(color, [1.0, 1.0, 1.0, color[3]], amount)
}

/// Replace a color's alpha channel.
fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}
