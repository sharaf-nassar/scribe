//! GPU-rendered prompt bar for AI terminal panes.
//!
//! Renders a background quad, icon glyphs, and truncated prompt text for
//! the first and latest prompts submitted in a supported AI coding session.
//! Also renders a `#N` message-count annotation and an elapsed-time counter
//! since the most recent prompt was sent, with optional AI context-window
//! usage in the right-edge cluster.

use std::time::{Duration, SystemTime};

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
/// Cell-width slot reserved for the elapsed-time counter. Sized for the
/// widest possible format ("12m 04s" / "18h 03m" — 7 characters) so digit
/// rollovers do not jitter the surrounding layout.
const TIMER_SLOT_CELLS: usize = 7;
/// Cell-width slot reserved for the AI context indicator. Sized for the
/// widest output (`▰▰▰ 100%`) so percentage digit changes do not move the
/// timer/count cluster.
const CONTEXT_SLOT_CELLS: usize = 8;
/// Cells used between the timer and count in the 1-message state: `" · "`.
const SEPARATOR_CELLS: usize = 3;
/// Cells of breathing room between the prompt-text run and the right-edge
/// cluster (count / timer).
const RIGHT_GUTTER_CELLS: usize = 2;

/// Unicode for the circle-dot (origin) icon.
const ICON_FIRST: char = '⊙';
/// Unicode for the right-arrow (latest) icon.
const ICON_LATEST: char = '→';
/// Unicode for the dismiss overlay icon.
const ICON_DISMISS: char = '×';
/// Middle-dot used as a typographic separator between timer and count
/// in the 1-message state.
const SEPARATOR_GLYPH: char = '·';
/// Hash glyph that prefixes the message count (`#4`).
const COUNT_PREFIX: char = '#';
/// Filled segment of the context-window level meter (BLACK PARALLELOGRAM).
const BAR_FULL: char = '\u{25B0}';
/// Empty segment of the context-window level meter (WHITE PARALLELOGRAM).
const BAR_EMPTY: char = '\u{25B1}';
/// Number of segments in the context level meter.
const CONTEXT_BAR_SEGMENTS: usize = 3;
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
    /// Rect occupied by the `#N` count text. Always present whenever the
    /// bar is visible (single- and multi-prompt states).
    pub count_text_rect: Rect,
    /// Rect occupied by the elapsed-time text. Sits left of the count in
    /// the 1-message state and directly under the count in the 2-message
    /// state.
    pub timer_text_rect: Rect,
    /// Rect occupied by the middle-dot separator between timer and count
    /// in the 1-message state. `None` in the 2-message state.
    pub separator_text_rect: Option<Rect>,
    /// Rect occupied by the optional AI context-window indicator. Sits to
    /// the right of the count so the timer remains paired with `#N`.
    pub context_text_rect: Option<Rect>,
    /// Rect occupied by the middle-dot separator between count and context.
    pub context_separator_rect: Option<Rect>,
    pub row_content_x: f32,
    pub first_line_width: f32,
    pub latest_line_width: Option<f32>,
}

#[derive(Clone, Copy)]
pub struct PromptContextIndicator {
    pub percent: u8,
    pub color: [f32; 4],
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
    pub context_indicator: Option<PromptContextIndicator>,
    /// Wall-clock time used to compute the elapsed-time counter. Threaded
    /// in (rather than read from `SystemTime::now()` inside the renderer)
    /// so tests can drive the counter deterministically.
    pub now: SystemTime,
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

    fn render_count_text(
        &mut self,
        rect: Rect,
        count: u32,
        text_color: [f32; 4],
        bg_color: [f32; 4],
    ) {
        let mut x = rect.x;
        let y = rect.y;
        x = self.emit_glyph(PromptGlyphRequest {
            ch: COUNT_PREFIX,
            x,
            y,
            fg_color: text_color,
            bg_color,
        });
        for ch in count.to_string().chars() {
            x = self.emit_glyph(PromptGlyphRequest { ch, x, y, fg_color: text_color, bg_color });
        }
    }

    fn render_timer_text(
        &mut self,
        rect: Rect,
        text: &str,
        text_color: [f32; 4],
        bg_color: [f32; 4],
    ) {
        let cell_w = self.metrics.cell_size.0;
        let text_width = cells_to_pixels(text.chars().count(), cell_w);
        // Right-align inside the slot so digit-width changes (e.g. 9 sec → 10 sec)
        // do not jitter the position of the rest of the bar.
        let mut x = rect.x + (rect.width - text_width).max(0.0);
        let y = rect.y;
        for ch in text.chars() {
            x = self.emit_glyph(PromptGlyphRequest { ch, x, y, fg_color: text_color, bg_color });
        }
    }

    fn render_separator(&mut self, rect: Rect, text_color: [f32; 4], bg_color: [f32; 4]) {
        let cell_w = self.metrics.cell_size.0;
        // Center the middle-dot inside the 3-cell slot (`" · "`).
        let x = rect.x + cell_w;
        let y = rect.y;
        self.emit_glyph(PromptGlyphRequest {
            ch: SEPARATOR_GLYPH,
            x,
            y,
            fg_color: text_color,
            bg_color,
        });
    }

    fn render_context_text(
        &mut self,
        rect: Rect,
        percent: u8,
        text_color: [f32; 4],
        bg_color: [f32; 4],
    ) {
        let text = format_context_label(percent);
        let cell_w = self.metrics.cell_size.0;
        let text_width = cells_to_pixels(text.chars().count(), cell_w);
        let mut x = rect.x + (rect.width - text_width).max(0.0);
        let y = rect.y;
        for ch in text.chars() {
            x = self.emit_glyph(PromptGlyphRequest { ch, x, y, fg_color: text_color, bg_color });
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

/// Compute the shared geometry for the prompt bar strip, rows, count, timer, and truncation widths.
pub fn compute_prompt_bar_layout(
    pane: &Pane,
    bar_rect: Rect,
    cell_size: (f32, f32),
    context_indicator: Option<PromptContextIndicator>,
) -> Option<PromptBarLayout> {
    if pane.prompt_count == 0 {
        return None;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let card_rect = bar_rect;
    let two_rows = pane.prompt_count >= 2;
    let RowGeometry { first: first_row_rect, latest: latest_row_rect, seam: seam_rect } =
        compute_row_geometry(card_rect, cell_h, two_rows);
    let row_content_x = card_rect.x;

    let cluster_right = card_rect.x + card_rect.width - ROW_SIDE_PAD;
    let count_cells = count_text_cells(pane.prompt_count);
    let count_width = cells_to_pixels(count_cells, cell_w);
    let timer_width = cells_to_pixels(TIMER_SLOT_CELLS, cell_w);
    let context_width =
        context_indicator.map_or(0.0, |_| cells_to_pixels(CONTEXT_SLOT_CELLS, cell_w));
    let separator_width = cells_to_pixels(SEPARATOR_CELLS, cell_w);
    let gutter_width = cells_to_pixels(RIGHT_GUTTER_CELLS, cell_w);

    let row1_text_y = first_row_rect.y + (first_row_rect.height - cell_h) * 0.5;
    // In two-row mode, count and context render on row 2 (under the latest
    // prompt) while the timer takes the row-1 right edge. In one-row mode,
    // everything sits on row 1.
    let count_context_text_y =
        latest_row_rect.map_or(row1_text_y, |latest| latest.y + (latest.height - cell_h) * 0.5);

    let count_context_cluster = first_row_cluster_layout(FirstRowClusterArgs {
        cluster_right,
        row_text_y: count_context_text_y,
        cell_h,
        count_width,
        context_width,
        separator_width,
    });

    let one_row_args = OneRowClusterArgs {
        cluster_right,
        count_text_rect: count_context_cluster.count_text_rect,
        row_text_y: count_context_cluster.text_y,
        cell_h,
        timer_width,
        separator_width,
        gutter_width,
    };
    let cluster = latest_row_rect.map_or_else(
        || one_row_cluster(one_row_args),
        |_latest| {
            two_row_cluster(TwoRowClusterArgs {
                timer_text_y: row1_text_y,
                cluster_right,
                count_text_rect: count_context_cluster.count_text_rect,
                cell_h,
                timer_width,
                gutter_width,
            })
        },
    );
    let RightCluster {
        timer_text_rect,
        separator_text_rect,
        first_right_reserved,
        latest_right_reserved,
    } = cluster;
    let (context_text_rect, context_separator_rect) = count_context_cluster
        .context
        .map_or((None, None), |(text, separator)| (Some(text), Some(separator)));

    let first_line_width = (card_rect.width - first_right_reserved).max(1.0);
    let latest_line_width =
        if two_rows { Some((card_rect.width - latest_right_reserved).max(1.0)) } else { None };

    Some(PromptBarLayout {
        card_rect,
        first_row_rect,
        latest_row_rect,
        seam_rect,
        count_text_rect: count_context_cluster.count_text_rect,
        timer_text_rect,
        separator_text_rect,
        context_text_rect,
        context_separator_rect,
        row_content_x,
        first_line_width,
        latest_line_width,
    })
}

/// First-row rect, optional second-row rect, and the seam between them.
struct RowGeometry {
    first: Rect,
    latest: Option<Rect>,
    seam: Option<Rect>,
}

#[derive(Clone, Copy)]
struct FirstRowClusterArgs {
    cluster_right: f32,
    row_text_y: f32,
    cell_h: f32,
    count_width: f32,
    context_width: f32,
    separator_width: f32,
}

#[derive(Clone, Copy)]
struct FirstRowCluster {
    text_y: f32,
    count_text_rect: Rect,
    context: Option<(Rect, Rect)>,
}

fn first_row_cluster_layout(args: FirstRowClusterArgs) -> FirstRowCluster {
    let context = context_after_count_layout(
        args.cluster_right,
        args.row_text_y,
        args.cell_h,
        args.context_width,
        args.separator_width,
    );
    let count_right = context.map_or(args.cluster_right, |(_, separator)| separator.x);
    let count_text_rect = Rect {
        x: count_right - args.count_width,
        y: args.row_text_y,
        width: args.count_width,
        height: args.cell_h,
    };
    FirstRowCluster { text_y: args.row_text_y, count_text_rect, context }
}

fn compute_row_geometry(card_rect: Rect, cell_h: f32, two_rows: bool) -> RowGeometry {
    let row_height = prompt_bar_row_height(cell_h);
    let first = Rect { x: card_rect.x, y: card_rect.y, width: card_rect.width, height: row_height };
    let latest = two_rows.then_some(Rect {
        x: card_rect.x,
        y: first.y + row_height + ROW_SEAM_H,
        width: card_rect.width,
        height: row_height,
    });
    let seam = latest.map(|latest_rect| Rect {
        x: card_rect.x,
        y: latest_rect.y - ROW_SEAM_H,
        width: card_rect.width,
        height: ROW_SEAM_H,
    });
    RowGeometry { first, latest, seam }
}

/// Geometry for the right-edge cluster (count, timer, optional separator)
/// plus how much horizontal width each row reserves for it.
struct RightCluster {
    timer_text_rect: Rect,
    separator_text_rect: Option<Rect>,
    first_right_reserved: f32,
    latest_right_reserved: f32,
}

#[derive(Clone, Copy)]
struct TwoRowClusterArgs {
    /// Vertical baseline of row 1's text — the timer's row in two-row mode.
    timer_text_y: f32,
    cluster_right: f32,
    /// Already-laid-out count rect on row 2.
    count_text_rect: Rect,
    cell_h: f32,
    timer_width: f32,
    gutter_width: f32,
}

#[derive(Clone, Copy)]
struct OneRowClusterArgs {
    cluster_right: f32,
    count_text_rect: Rect,
    row_text_y: f32,
    cell_h: f32,
    timer_width: f32,
    separator_width: f32,
    gutter_width: f32,
}

/// 2-message layout: count + context sit on row 2 (already laid out by the
/// caller); the timer takes row 1, right-anchored to `cluster_right` so its
/// trailing edge aligns with the row-2 cluster.
fn two_row_cluster(args: TwoRowClusterArgs) -> RightCluster {
    let timer_text_rect = Rect {
        x: args.cluster_right - args.timer_width,
        y: args.timer_text_y,
        width: args.timer_width,
        height: args.cell_h,
    };
    RightCluster {
        timer_text_rect,
        separator_text_rect: None,
        first_right_reserved: (args.cluster_right - timer_text_rect.x) + args.gutter_width,
        latest_right_reserved: (args.cluster_right - args.count_text_rect.x) + args.gutter_width,
    }
}

/// 1-message layout: row 1 carries `<timer>  ·  #N`, right-aligned. Row 2
/// does not exist, so it reserves zero width.
fn one_row_cluster(args: OneRowClusterArgs) -> RightCluster {
    let separator_text_rect = Rect {
        x: args.count_text_rect.x - args.separator_width,
        y: args.row_text_y,
        width: args.separator_width,
        height: args.cell_h,
    };
    let timer_text_rect = Rect {
        x: separator_text_rect.x - args.timer_width,
        y: args.row_text_y,
        width: args.timer_width,
        height: args.cell_h,
    };
    RightCluster {
        timer_text_rect,
        separator_text_rect: Some(separator_text_rect),
        first_right_reserved: (args.cluster_right - timer_text_rect.x) + args.gutter_width,
        latest_right_reserved: 0.0,
    }
}

fn context_after_count_layout(
    cluster_right: f32,
    row_text_y: f32,
    cell_h: f32,
    context_width: f32,
    separator_width: f32,
) -> Option<(Rect, Rect)> {
    if context_width <= 0.0 {
        return None;
    }

    let context_text_rect = Rect {
        x: cluster_right - context_width,
        y: row_text_y,
        width: context_width,
        height: cell_h,
    };
    let separator_text_rect = Rect {
        x: context_text_rect.x - separator_width,
        y: row_text_y,
        width: separator_width,
        height: cell_h,
    };
    Some((context_text_rect, separator_text_rect))
}

/// Number of cells the `#N` count occupies (`#` + decimal digits of `count`).
fn count_text_cells(count: u32) -> usize {
    let digits = if count == 0 { 1 } else { (count.ilog10() as usize) + 1 };
    1 + digits
}

/// Format an elapsed `Duration` into the prompt-bar display string.
///
/// Thresholds:
/// - `< 60s`: `"X sec"` — counts up second-by-second from a fresh prompt.
/// - `< 1h`: `"Xm YYs"` — minutes (un-padded) and seconds (zero-padded).
/// - `>= 1h`: `"Xh YYm"` — hours (un-padded) and minutes (zero-padded).
///
/// All formats fit within `TIMER_SLOT_CELLS`; the rendered text is
/// right-aligned inside that slot to keep digit rollovers jitter-free.
#[must_use]
pub fn format_elapsed(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    if total_secs < 60 {
        format!("{total_secs} sec")
    } else if total_secs < 3600 {
        let minutes = total_secs / 60;
        let seconds = total_secs % 60;
        format!("{minutes}m {seconds:02}s")
    } else {
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        format!("{hours}h {minutes:02}m")
    }
}

fn format_context_label(percent: u8) -> String {
    let percent = percent.min(100);
    let filled =
        (usize::from(percent) * CONTEXT_BAR_SEGMENTS).div_ceil(100).min(CONTEXT_BAR_SEGMENTS);
    let mut label = String::with_capacity(CONTEXT_SLOT_CELLS);
    for _ in 0..filled {
        label.push(BAR_FULL);
    }
    for _ in filled..CONTEXT_BAR_SEGMENTS {
        label.push(BAR_EMPTY);
    }
    label.push(' ');
    label.push_str(&percent.to_string());
    label.push('%');
    label
}

/// Compute the elapsed `Duration` from `since` to `now`, clamped to zero
/// when the wall clock has moved backwards (DST shift, NTP correction).
fn elapsed_since(now: SystemTime, since: SystemTime) -> Duration {
    now.duration_since(since).unwrap_or(Duration::ZERO)
}

/// Compute the formatted elapsed-time string for `pane`, if a prompt has
/// been received and a timestamp is recorded. Returns `None` when nothing
/// should be drawn (no prompt, or restored from a snapshot that predates
/// the timestamp field).
///
/// When `pane.latest_prompt_finished_at` is `Some`, the timer is frozen
/// at that instant — the displayed elapsed value reflects the time from
/// prompt submission to LLM finish (or the AI state transition that
/// captured the freeze) rather than wall-clock time since the prompt.
fn pane_elapsed_text(pane: &Pane, now: SystemTime) -> Option<String> {
    let since = pane.latest_prompt_at?;
    let reference = pane.latest_prompt_finished_at.unwrap_or(now);
    Some(format_elapsed(elapsed_since(reference, since)))
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
        context_indicator,
        now,
        resolve_glyph,
    } = context;

    if pane.prompt_count == 0 {
        return;
    }

    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    let Some(layout) = compute_prompt_bar_layout(pane, bar_rect, cell_size, context_indicator)
    else {
        return;
    };
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

    render_right_cluster(RightClusterRenderArgs {
        renderer: &mut renderer,
        layout: &layout,
        pane,
        colors,
        hover,
        active,
        context_indicator,
        now,
    });
}

struct RightClusterRenderArgs<'a, 'b> {
    renderer: &'a mut PromptRenderer<'b>,
    layout: &'a PromptBarLayout,
    pane: &'a Pane,
    colors: &'a PromptBarColors,
    hover: Option<PromptBarHover>,
    active: Option<PromptBarHover>,
    context_indicator: Option<PromptContextIndicator>,
    now: SystemTime,
}

/// Right-edge cluster: in two-row mode the timer alone sits on row 1 and
/// the count + context cluster sits on row 2; in one-row mode everything
/// sits on row 1. Each glyph uses its row's effective background (which
/// tracks the row's hover/active state) so the cluster cells blend with
/// the row tint above instead of punching through it.
fn render_right_cluster(args: RightClusterRenderArgs<'_, '_>) {
    let RightClusterRenderArgs {
        renderer,
        layout,
        pane,
        colors,
        hover,
        active,
        context_indicator,
        now,
    } = args;
    let two_rows = layout.latest_row_rect.is_some();
    let row1_bg = effective_row_bg(
        colors.first_row_bg,
        prompt_row_state(hover, active, PromptBarHover::First),
    );
    let row2_bg = effective_row_bg(
        colors.second_row_bg,
        prompt_row_state(hover, active, PromptBarHover::Latest),
    );
    let count_context_bg = if two_rows { row2_bg } else { row1_bg };
    let timer_bg = row1_bg;
    let count_color = with_alpha(colors.text, 0.62);
    let timer_color = with_alpha(colors.text, 0.42);
    let separator_color = with_alpha(colors.text, 0.28);
    renderer.render_count_text(
        layout.count_text_rect,
        pane.prompt_count,
        count_color,
        count_context_bg,
    );
    if let Some(elapsed_text) = pane_elapsed_text(pane, now) {
        renderer.render_timer_text(layout.timer_text_rect, &elapsed_text, timer_color, timer_bg);
        if let Some(sep_rect) = layout.separator_text_rect {
            renderer.render_separator(sep_rect, separator_color, timer_bg);
        }
    }
    if let (Some(indicator), Some(context_rect)) = (context_indicator, layout.context_text_rect) {
        renderer.render_context_text(
            context_rect,
            indicator.percent,
            indicator.color,
            count_context_bg,
        );
        if let Some(sep_rect) = layout.context_separator_rect {
            renderer.render_separator(sep_rect, separator_color, count_context_bg);
        }
    }
}

fn effective_row_bg(base_bg: [f32; 4], state: PromptRowState) -> [f32; 4] {
    match state {
        PromptRowState::Idle => base_bg,
        PromptRowState::Hovered => lift_color(base_bg, 0.035),
        PromptRowState::Active => lift_color(base_bg, 0.07),
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
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size, None)?;
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
    context_indicator: Option<PromptContextIndicator>,
) -> Option<f32> {
    let layout = compute_prompt_bar_layout(pane, bar_rect, cell_size, context_indicator)?;
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

/// Convert a cell count to pixel width using the same lossy-but-bounded
/// `usize → u16 → f32` path as [`prompt_text_width`]. Caller-side casts
/// from `usize` directly to `f32` would trip clippy's
/// `cast_precision_loss` even though prompt-bar widths cannot realistically
/// exceed `u16::MAX` cells.
fn cells_to_pixels(cells: usize, cell_w: f32) -> f32 {
    prompt_text_width(cells, cell_w)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn d(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn format_elapsed_under_a_minute_uses_x_sec() {
        assert_eq!(format_elapsed(d(0)), "0 sec");
        assert_eq!(format_elapsed(d(3)), "3 sec");
        assert_eq!(format_elapsed(d(47)), "47 sec");
        assert_eq!(format_elapsed(d(59)), "59 sec");
    }

    #[test]
    fn format_elapsed_under_an_hour_uses_xm_yys() {
        assert_eq!(format_elapsed(d(60)), "1m 00s");
        assert_eq!(format_elapsed(d(61)), "1m 01s");
        assert_eq!(format_elapsed(d(4 * 60 + 22)), "4m 22s");
        assert_eq!(format_elapsed(d(12 * 60 + 4)), "12m 04s");
        assert_eq!(format_elapsed(d(59 * 60 + 59)), "59m 59s");
    }

    #[test]
    fn format_elapsed_one_hour_or_more_uses_xh_yym() {
        assert_eq!(format_elapsed(d(3600)), "1h 00m");
        assert_eq!(format_elapsed(d(2 * 3600 + 15 * 60)), "2h 15m");
        assert_eq!(format_elapsed(d(18 * 3600 + 3 * 60)), "18h 03m");
    }

    #[test]
    fn format_elapsed_widths_fit_timer_slot() {
        // The reserved slot must accommodate the widest output of every
        // threshold so the right-anchored cluster never has to grow.
        let samples = [d(0), d(9), d(59), d(60), d(599), d(3599), d(3600), d(99 * 3600)];
        for sample in samples {
            let s = format_elapsed(sample);
            assert!(
                s.chars().count() <= TIMER_SLOT_CELLS,
                "format_elapsed({sample:?}) = {s:?} exceeds TIMER_SLOT_CELLS={TIMER_SLOT_CELLS}",
            );
        }
    }

    #[test]
    fn count_text_cells_includes_hash_prefix() {
        assert_eq!(count_text_cells(0), 2); // "#0"
        assert_eq!(count_text_cells(1), 2); // "#1"
        assert_eq!(count_text_cells(9), 2);
        assert_eq!(count_text_cells(10), 3); // "#10"
        assert_eq!(count_text_cells(99), 3);
        assert_eq!(count_text_cells(100), 4);
    }

    #[test]
    fn format_context_label_renders_segmented_meter() {
        assert_eq!(format_context_label(0), "▱▱▱ 0%");
        assert_eq!(format_context_label(1), "▰▱▱ 1%");
        assert_eq!(format_context_label(33), "▰▱▱ 33%");
        assert_eq!(format_context_label(34), "▰▰▱ 34%");
        assert_eq!(format_context_label(70), "▰▰▰ 70%");
        assert_eq!(format_context_label(99), "▰▰▰ 99%");
        assert_eq!(format_context_label(100), "▰▰▰ 100%");
    }

    #[test]
    fn format_context_label_widths_fit_context_slot() {
        for percent in 0u8..=100 {
            let s = format_context_label(percent);
            assert!(
                s.chars().count() <= CONTEXT_SLOT_CELLS,
                "format_context_label({percent}) = {s:?} exceeds CONTEXT_SLOT_CELLS={CONTEXT_SLOT_CELLS}",
            );
        }
    }

    #[test]
    fn format_context_label_clamps_above_100() {
        assert_eq!(format_context_label(200), "▰▰▰ 100%");
        assert_eq!(format_context_label(u8::MAX), "▰▰▰ 100%");
    }

    #[test]
    fn elapsed_since_clamps_clock_skew() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        // since is in the future relative to now → should clamp to ZERO.
        let since = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        assert_eq!(elapsed_since(now, since), Duration::ZERO);
        // since in the past → straightforward subtraction.
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(40);
        assert_eq!(elapsed_since(now, earlier), Duration::from_secs(60));
    }
}
