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

/// Horizontal padding within a prompt row.
pub const ROW_SIDE_PAD: f32 = 14.0;
const ROW_SEAM_H: f32 = 1.0;
/// Minimum prompt-row height.
pub const ROW_MIN_HEIGHT: f32 = 28.0;
/// Gap between icon and text.
pub const ICON_TEXT_GAP: f32 = 10.0;
/// Count badge height.
pub const COUNT_BADGE_H: f32 = 18.0;
/// Horizontal padding inside the count badge.
pub const COUNT_BADGE_PAD_X: f32 = 8.0;
/// Minimum count badge width.
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

#[derive(Clone, Copy)]
struct PromptRenderMetrics {
    row_content_x: f32,
    cell_size: (f32, f32),
    glyph_size: [f32; 2],
}

#[derive(Clone, Copy)]
struct PromptRowColors {
    icon: [f32; 4],
    text: [f32; 4],
    base_bg: [f32; 4],
    hover_bg: [f32; 4],
    active_bg: [f32; 4],
}

#[derive(Clone, Copy)]
enum PromptRowState {
    Idle,
    Hovered,
    Active,
}

struct PromptRowSpec<'a> {
    row_rect: Rect,
    icon: char,
    text: &'a str,
    line_width: f32,
    colors: PromptRowColors,
    state: PromptRowState,
}

#[derive(Clone, Copy)]
struct DismissOverlayStyle {
    background: [f32; 4],
    foreground: [f32; 4],
}

pub struct PromptBarRenderContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub pane: &'a Pane,
    pub bar_rect: Rect,
    pub cell_size: (f32, f32),
    pub glyph_size: [f32; 2],
    pub hover: Option<PromptBarHover>,
    pub active: Option<PromptBarHover>,
    pub colors: &'a PromptBarColors,
    pub resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
}

#[derive(Clone, Copy)]
struct PromptLineRequest<'a> {
    icon: char,
    text: &'a str,
    bar_x: f32,
    y: f32,
    bar_width: f32,
    icon_color: [f32; 4],
    text_color: [f32; 4],
    bg_color: [f32; 4],
}

#[derive(Clone, Copy)]
struct PromptGlyphRequest {
    ch: char,
    x: f32,
    y: f32,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
}

struct PromptRenderer<'a> {
    out: &'a mut Vec<CellInstance>,
    metrics: PromptRenderMetrics,
    resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
}

impl PromptRenderer<'_> {
    fn render_prompt_row(&mut self, spec: &PromptRowSpec<'_>) {
        let cell_h = self.metrics.cell_size.1;

        push_solid_rect(self.out, spec.row_rect, spec.colors.base_bg);
        match spec.state {
            PromptRowState::Hovered => {
                push_solid_rect(self.out, spec.row_rect, spec.colors.hover_bg);
            }
            PromptRowState::Active => {
                push_solid_rect(self.out, spec.row_rect, spec.colors.active_bg);
            }
            PromptRowState::Idle => {}
        }

        self.render_prompt_line(PromptLineRequest {
            icon: spec.icon,
            text: spec.text,
            bar_x: self.metrics.row_content_x,
            y: spec.row_rect.y + (spec.row_rect.height - cell_h) * 0.5,
            bar_width: spec.line_width,
            icon_color: spec.colors.icon,
            text_color: spec.colors.text,
            bg_color: spec.colors.base_bg,
        });
    }

    fn render_dismiss_overlay(&mut self, layout: &PromptBarLayout, style: DismissOverlayStyle) {
        let (cell_w, cell_h) = self.metrics.cell_size;
        let dismiss_rect = dismiss_overlay_rect(layout, self.metrics.cell_size);

        push_solid_rect(self.out, dismiss_rect, style.background);
        let dismiss_x = dismiss_rect.x + (dismiss_rect.width - cell_w) * 0.5;
        let dismiss_y = dismiss_rect.y + (dismiss_rect.height - cell_h) * 0.5;
        self.emit_glyph(PromptGlyphRequest {
            ch: ICON_DISMISS,
            x: dismiss_x,
            y: dismiss_y,
            fg_color: style.foreground,
            bg_color: style.background,
        });
    }

    fn render_prompt_line(&mut self, request: PromptLineRequest<'_>) {
        let mut x = request.bar_x + ROW_SIDE_PAD;
        let max_x = request.bar_x + request.bar_width - ROW_SIDE_PAD;

        x = self.emit_glyph(PromptGlyphRequest {
            ch: request.icon,
            x,
            y: request.y,
            fg_color: request.icon_color,
            bg_color: request.bg_color,
        });
        x += ICON_TEXT_GAP;

        let chars: Vec<char> = request.text.chars().collect();
        let available_chars = prompt_chars_in_width(max_x - x, self.metrics.cell_size.0);

        let needs_ellipsis = chars.len() > available_chars;
        let visible_count =
            if needs_ellipsis { available_chars.saturating_sub(1) } else { chars.len() };

        for &ch in chars.get(..visible_count).unwrap_or(&[]) {
            if x + self.metrics.cell_size.0 > max_x {
                break;
            }
            x = self.emit_glyph(PromptGlyphRequest {
                ch,
                x,
                y: request.y,
                fg_color: request.text_color,
                bg_color: request.bg_color,
            });
        }

        if needs_ellipsis && x + self.metrics.cell_size.0 <= max_x {
            self.emit_glyph(PromptGlyphRequest {
                ch: '…',
                x,
                y: request.y,
                fg_color: request.text_color,
                bg_color: request.bg_color,
            });
        }
    }

    fn render_count_badge(&mut self, rect: Rect, count: u32, colors: &PromptBarColors) {
        let (cell_w, cell_h) = self.metrics.cell_size;
        let badge_fill = mix(colors.second_row_bg, colors.text, 0.06);
        let badge_text = with_alpha(colors.text, 0.96);

        push_solid_rect(self.out, rect, badge_fill);

        let text = count.to_string();
        let text_width = text.chars().fold(0.0, |width, _| width + cell_w);
        let mut x = rect.x + (rect.width - text_width).max(0.0) * 0.5;
        let y = rect.y + (rect.height - cell_h) * 0.5;
        for ch in text.chars() {
            x = self.emit_glyph(PromptGlyphRequest {
                ch,
                x,
                y,
                fg_color: badge_text,
                bg_color: badge_fill,
            });
        }
    }

    fn emit_glyph(&mut self, request: PromptGlyphRequest) -> f32 {
        let (uv_min, uv_max) = (self.resolve_glyph)(request.ch);
        self.out.push(CellInstance {
            pos: [request.x, request.y],
            size: self.metrics.glyph_size,
            uv_min,
            uv_max,
            fg_color: request.fg_color,
            bg_color: request.bg_color,
            corner_radius: 0.0,
        });
        request.x + self.metrics.cell_size.0
    }
}

/// Compute the row height used by the prompt-bar strip layout.
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
pub fn prompt_bar_height(prompt_count: u32, cell_height: f32) -> f32 {
    if prompt_count == 0 || cell_height <= 0.0 {
        return 0.0;
    }

    let row_height = prompt_bar_row_height(cell_height);
    if prompt_count >= 2 { row_height * 2.0 + ROW_SEAM_H } else { row_height }
}

/// Compute the shared geometry for the prompt bar strip, rows, badge, and truncation widths.
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
pub fn render_prompt_bar(context: PromptBarRenderContext<'_>) {
    let PromptBarRenderContext {
        out,
        pane,
        bar_rect,
        cell_size,
        glyph_size,
        hover,
        active,
        colors,
        resolve_glyph,
    } = context;

    if pane.prompt_count == 0 {
        return;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    let Some(layout) = compute_prompt_bar_layout(pane, bar_rect, cell_size) else { return };
    let metrics =
        PromptRenderMetrics { row_content_x: layout.row_content_x, cell_size, glyph_size };
    let seam_color = with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.5), 0.20);
    let mut renderer = PromptRenderer { out, metrics, resolve_glyph };

    if let Some(spec) = first_prompt_row_spec(pane, &layout, colors, hover, active) {
        renderer.render_prompt_row(&spec);
    }

    if let Some(spec) = latest_prompt_row_spec(pane, &layout, colors, hover, active) {
        renderer.render_prompt_row(&spec);
    }

    if let Some(seam_rect) = layout.seam_rect {
        push_solid_rect(renderer.out, seam_rect, seam_color);
    }

    let show_dismiss_overlay = hover.is_some() || active == Some(PromptBarHover::DismissButton);
    if show_dismiss_overlay {
        let style = dismiss_overlay_style(*colors, active == Some(PromptBarHover::DismissButton));
        renderer.render_dismiss_overlay(&layout, style);
    }

    if let Some(badge_rect) = layout.count_badge_rect {
        renderer.render_count_badge(badge_rect, pane.prompt_count, colors);
    }
}

fn first_prompt_row_spec<'a>(
    pane: &'a Pane,
    layout: &PromptBarLayout,
    colors: &PromptBarColors,
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
) -> Option<PromptRowSpec<'a>> {
    let text = pane.first_prompt.as_deref()?;
    Some(PromptRowSpec {
        row_rect: layout.first_row_rect,
        icon: ICON_FIRST,
        text,
        line_width: layout.first_line_width,
        colors: row_colors(colors.icon_first, colors.text, colors.first_row_bg),
        state: prompt_row_state(hover, active, PromptBarHover::First),
    })
}

fn latest_prompt_row_spec<'a>(
    pane: &'a Pane,
    layout: &PromptBarLayout,
    colors: &PromptBarColors,
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
) -> Option<PromptRowSpec<'a>> {
    let row_rect = layout.latest_row_rect?;
    let text = pane.latest_prompt.as_deref()?;
    Some(PromptRowSpec {
        row_rect,
        icon: ICON_LATEST,
        text,
        line_width: layout.latest_line_width.unwrap_or(layout.first_line_width),
        colors: row_colors(colors.icon_latest, colors.text, colors.second_row_bg),
        state: prompt_row_state(hover, active, PromptBarHover::Latest),
    })
}

fn row_colors(icon: [f32; 4], text: [f32; 4], base_bg: [f32; 4]) -> PromptRowColors {
    PromptRowColors {
        icon,
        text,
        base_bg,
        hover_bg: lift_color(base_bg, 0.035),
        active_bg: lift_color(base_bg, 0.07),
    }
}

fn prompt_row_state(
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
    target: PromptBarHover,
) -> PromptRowState {
    if active == Some(target) {
        PromptRowState::Active
    } else if hover == Some(target) {
        PromptRowState::Hovered
    } else {
        PromptRowState::Idle
    }
}

fn dismiss_overlay_style(colors: PromptBarColors, active: bool) -> DismissOverlayStyle {
    DismissOverlayStyle {
        background: if active {
            with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.38), 1.0)
        } else {
            with_alpha(mix(colors.first_row_bg, colors.second_row_bg, 0.28), 1.0)
        },
        foreground: if active {
            with_alpha(colors.text, 1.0)
        } else {
            with_alpha(colors.text, 0.94)
        },
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
pub fn is_prompt_truncated(text: &str, bar_width: f32, cell_w: f32) -> bool {
    let usable = bar_width - ROW_SIDE_PAD * 2.0 - cell_w - ICON_TEXT_GAP;
    let text_width = prompt_text_width(text.chars().count(), cell_w);
    text_width > usable
}

fn prompt_text_width(char_count: usize, cell_w: f32) -> f32 {
    f32::from(u16::try_from(char_count).unwrap_or(u16::MAX)) * cell_w
}

fn prompt_chars_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || !width.is_finite() || width <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while prompt_text_width(high, cell_w) <= width && high < usize::from(u16::MAX) {
        low = high;
        high = high.saturating_mul(2).min(usize::from(u16::MAX));
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if prompt_text_width(mid, cell_w) <= width {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
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
