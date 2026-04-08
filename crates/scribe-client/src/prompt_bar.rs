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

/// Floating-card inset on the X axis.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_INSET_X: f32 = 10.0;
/// Floating-card inset on the Y axis.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_INSET_Y: f32 = 6.0;
/// Floating-card corner radius used by later renderer work.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const CARD_RADIUS: f32 = 12.0;
/// Horizontal padding within a prompt row.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_SIDE_PAD: f32 = 14.0;
/// Vertical gap between stacked prompt rows.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_GAP: f32 = 4.0;
/// Minimum prompt-row height.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const ROW_MIN_HEIGHT: f32 = 28.0;
/// Dismiss capsule width.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const DISMISS_CAPSULE_W: f32 = 24.0;
/// Dismiss capsule height.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const DISMISS_CAPSULE_H: f32 = 24.0;
/// Left inset from the card edge to the dismiss capsule.
#[allow(dead_code, reason = "shared geometry constant consumed by later renderer work")]
pub const DISMISS_CAPSULE_X: f32 = 10.0;
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

/// Bottom padding below the last line (used in `Pane::prompt_bar_height`).
#[allow(dead_code, reason = "documents the prompt bar layout constant used in pane.rs")]
const BOTTOM_PAD: f32 = 8.0;
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

/// Shared prompt-bar geometry for later rendering and hit-testing.
#[allow(dead_code, reason = "shared geometry struct consumed by later renderer work")]
#[derive(Clone, Copy)]
pub struct PromptBarLayout {
    pub card_rect: Rect,
    pub first_row_rect: Rect,
    pub latest_row_rect: Option<Rect>,
    pub seam_rect: Option<Rect>,
    pub dismiss_rect: Rect,
    pub count_badge_rect: Option<Rect>,
    pub row_content_x: f32,
    pub first_line_width: f32,
    pub latest_line_width: Option<f32>,
}

/// Compute the row height used by the future floating-card layout.
#[allow(dead_code, reason = "shared geometry helper consumed by later renderer work")]
fn prompt_bar_row_height(cell_height: f32) -> f32 {
    (cell_height + 10.0).max(ROW_MIN_HEIGHT)
}

/// Compute the dismiss capsule and its bridged hit-zone geometry.
fn dismiss_affordance_rects(
    card_rect: Rect,
    cell_height: f32,
    has_latest_row: bool,
) -> (Rect, Rect) {
    let capsule_height = DISMISS_CAPSULE_H.max(cell_height + 4.0);
    let capsule_width = DISMISS_CAPSULE_W.max(capsule_height * 1.15);
    let dismiss_center_y = if has_latest_row {
        card_rect.y + prompt_bar_row_height(cell_height) + ROW_GAP * 0.5
    } else {
        card_rect.y + prompt_bar_height(1, cell_height) * 0.5 - CARD_INSET_Y
    };
    let dismiss_rect = Rect {
        x: card_rect.x + DISMISS_CAPSULE_X,
        y: dismiss_center_y - capsule_height * 0.5,
        width: capsule_width,
        height: capsule_height,
    };
    let bridge_inset_y = (capsule_height * 0.18).max(4.0);
    let bridge_rect = Rect {
        x: (dismiss_rect.x - DISMISS_CAPSULE_X).max(card_rect.x),
        y: dismiss_rect.y + bridge_inset_y,
        width: (DISMISS_CAPSULE_X + dismiss_rect.width * 0.58).max(1.0),
        height: (dismiss_rect.height - bridge_inset_y * 2.0).max(1.0),
    };

    (dismiss_rect, bridge_rect)
}

/// Compute the line container start and gutter reserved by the dismiss affordance.
fn dismiss_content_lane(card_rect: Rect, dismiss_rect: Rect) -> (f32, f32) {
    const DISMISS_CONTENT_GAP: f32 = 8.0;

    let line_x = dismiss_rect.x + dismiss_rect.width + DISMISS_CONTENT_GAP - ROW_SIDE_PAD;
    let reserved_width = (line_x - card_rect.x).max(0.0);
    (line_x, reserved_width)
}

/// Compute the prompt bar height for the current live strip renderer.
#[allow(dead_code, reason = "shared geometry helper consumed by later pane/layout work")]
#[allow(clippy::cast_precision_loss, reason = "small integer counts map cleanly to f32")]
pub fn prompt_bar_height(prompt_count: u32, cell_height: f32) -> f32 {
    if prompt_count == 0 || cell_height <= 0.0 {
        return 0.0;
    }

    let row_height = prompt_bar_row_height(cell_height);
    let stacked_height = if prompt_count >= 2 { row_height * 2.0 + ROW_GAP } else { row_height };

    stacked_height + CARD_INSET_Y * 2.0
}

/// Compute the shared geometry for the prompt bar's card, rows, badge, and dismiss capsule.
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
    let card_rect = Rect {
        x: bar_rect.x + CARD_INSET_X,
        y: bar_rect.y + CARD_INSET_Y,
        width: (bar_rect.width - CARD_INSET_X * 2.0).max(1.0),
        height: (bar_rect.height - CARD_INSET_Y * 2.0).max(1.0),
    };
    let first_row_rect =
        Rect { x: card_rect.x, y: card_rect.y, width: card_rect.width, height: row_height };
    let latest_row_rect = (pane.prompt_count >= 2).then_some(Rect {
        x: card_rect.x,
        y: first_row_rect.y + row_height + ROW_GAP,
        width: card_rect.width,
        height: row_height,
    });
    let seam_rect = latest_row_rect.map(|latest| Rect {
        x: card_rect.x + CARD_RADIUS,
        y: latest.y - ROW_GAP * 0.5,
        width: (card_rect.width - CARD_RADIUS * 2.0).max(1.0),
        height: 1.0,
    });
    let (dismiss_rect, _) = dismiss_affordance_rects(card_rect, cell_h, latest_row_rect.is_some());
    let (row_content_x, dismiss_reserved_width) = dismiss_content_lane(card_rect, dismiss_rect);
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
    let first_line_width = (card_rect.width - dismiss_reserved_width - badge_reserved).max(1.0);
    let latest_line_width = if pane.prompt_count >= 2 {
        Some((card_rect.width - dismiss_reserved_width).max(1.0))
    } else {
        None
    };

    Some(PromptBarLayout {
        card_rect,
        first_row_rect,
        latest_row_rect,
        seam_rect,
        dismiss_rect,
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
    let shell_bg = mix(colors.bg, colors.first_row_bg, 0.14);
    let first_row_bg = mix(shell_bg, colors.first_row_bg, 0.50);
    let latest_row_bg = mix(shell_bg, colors.bg, 0.08);
    let hover_overlay = with_alpha([1.0, 1.0, 1.0, 1.0], 0.035);
    let active_overlay = with_alpha([1.0, 1.0, 1.0, 1.0], 0.07);
    let seam_color = with_alpha(colors.text, 0.08);
    let shadow_color = with_alpha(colors.text, 0.06);

    push_rounded_rect(
        out,
        Rect {
            x: layout.card_rect.x,
            y: layout.card_rect.y + 1.0,
            width: layout.card_rect.width,
            height: layout.card_rect.height,
        },
        shadow_color,
        CARD_RADIUS,
    );
    push_rounded_rect(out, layout.card_rect, shell_bg, CARD_RADIUS);

    if let Some(text) = &pane.first_prompt {
        let first_bg = Rect {
            x: layout.first_row_rect.x + 1.0,
            y: layout.first_row_rect.y + 1.0,
            width: (layout.first_row_rect.width - 2.0).max(1.0),
            height: (layout.first_row_rect.height - 1.5).max(1.0),
        };
        push_rounded_rect(out, first_bg, first_row_bg, CARD_RADIUS - 2.0);
        if hover == Some(PromptBarHover::First) {
            push_rounded_rect(out, first_bg, hover_overlay, CARD_RADIUS - 2.0);
        }
        if active == Some(PromptBarHover::First) {
            push_rounded_rect(out, first_bg, active_overlay, CARD_RADIUS - 2.0);
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
        let latest_bg = Rect {
            x: latest_row_rect.x + 1.0,
            y: latest_row_rect.y,
            width: (latest_row_rect.width - 2.0).max(1.0),
            height: (latest_row_rect.height - 1.0).max(1.0),
        };
        push_rounded_rect(out, latest_bg, latest_row_bg, CARD_RADIUS - 2.0);
        if hover == Some(PromptBarHover::Latest) {
            push_rounded_rect(out, latest_bg, hover_overlay, CARD_RADIUS - 2.0);
        }
        if active == Some(PromptBarHover::Latest) {
            push_rounded_rect(out, latest_bg, active_overlay, CARD_RADIUS - 2.0);
        }
        if let Some(seam_rect) = layout.seam_rect {
            push_solid_rect(out, seam_rect, seam_color);
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

    render_dismiss_capsule(
        out,
        layout.card_rect,
        layout.latest_row_rect.is_some(),
        hover == Some(PromptBarHover::DismissButton),
        active == Some(PromptBarHover::DismissButton),
        colors,
        cell_w,
        cell_h,
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
    let badge_fill = mix(colors.bg, colors.first_row_bg, 0.32);
    let badge_text = with_alpha(colors.text, 0.92);

    push_rounded_rect(out, rect, badge_fill, COUNT_BADGE_H * 0.5);

    let text = count.to_string();
    let text_width = text.chars().fold(0.0, |width, _| width + cell_w);
    let mut x = rect.x + (rect.width - text_width).max(0.0) * 0.5;
    let y = rect.y + (rect.height - cell_h) * 0.5;
    for ch in text.chars() {
        x = emit_glyph(out, ch, x, y, badge_text, badge_fill, cell_w, glyph_size, resolve_glyph);
    }
}

/// Render the bridged dismiss capsule and its glyph.
#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "capsule renderer needs geometry, colors, size, and glyph resolver"
)]
fn render_dismiss_capsule(
    out: &mut Vec<CellInstance>,
    card_rect: Rect,
    has_latest_row: bool,
    hovered: bool,
    active: bool,
    colors: &PromptBarColors,
    cell_w: f32,
    cell_h: f32,
    glyph_size: [f32; 2],
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let (rect, bridge_rect) = dismiss_affordance_rects(card_rect, cell_h, has_latest_row);
    let bridge_bg = mix(colors.bg, colors.first_row_bg, 0.26);
    let capsule_bg = if active {
        mix(bridge_bg, colors.text, 0.10)
    } else if hovered {
        mix(bridge_bg, colors.text, 0.06)
    } else {
        bridge_bg
    };

    push_rounded_rect(out, bridge_rect, bridge_bg, bridge_rect.height * 0.5);
    push_rounded_rect(out, rect, capsule_bg, rect.height * 0.5);

    let dismiss_fg =
        if hovered { mix(colors.text, colors.bg, 0.08) } else { mix(colors.text, colors.bg, 0.28) };
    let dismiss_x = rect.x + (rect.width - cell_w) * 0.5;
    let dismiss_y = rect.y + (rect.height - cell_h) * 0.5;
    emit_glyph(
        out,
        ICON_DISMISS,
        dismiss_x,
        dismiss_y,
        dismiss_fg,
        capsule_bg,
        cell_w,
        glyph_size,
        resolve_glyph,
    );
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

    let (_, dismiss_bridge_rect) =
        dismiss_affordance_rects(layout.card_rect, cell_size.1, layout.latest_row_rect.is_some());

    if layout.dismiss_rect.contains(mouse_x, mouse_y)
        || dismiss_bridge_rect.contains(mouse_x, mouse_y)
    {
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

/// Push a rounded rectangle into `out`.
fn push_rounded_rect(out: &mut Vec<CellInstance>, rect: Rect, color: [f32; 4], radius: f32) {
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
        corner_radius: radius.min(rect.width * 0.5).min(rect.height * 0.5),
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

/// Replace a color's alpha channel.
fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}
