//! Per-pane AI prompt bar for the GPUI client.
//!
//! The winit client rendered the prompt bar by emitting GPU quads
//! with hand-placed glyph geometry (`compute_prompt_bar_layout` and its cluster
//! math). The GPUI rebuild keeps the **display-independent** pieces byte-for-byte
//! — the elapsed-timer formatting and its freeze-on-AI-stop semantics, the
//! segmented context-window meter label, the `#N` count, the strip height, the
//! truncation predicate, and the hover target enum — and lowers the visuals onto
//! a GPUI flex strip via [`build_model`] + [`render`], the same split the
//! [`crate::status_bar`] and [`crate::tab_bar`] ports use.
//!
//! The elapsed timer freezes at [`PromptBarData::latest_prompt_finished_at`]
//! when the AI stops, so the displayed value reflects prompt-to-finish duration
//! rather than wall-clock time since submission — verified by
//! `#[gpui::test]` without a live window because the reference clock is threaded
//! in rather than read from `SystemTime::now()` inside the renderer.

use std::time::{Duration, SystemTime};

use gpui::{Rgba, Role, div, prelude::*, px};

use crate::opacity::scale_slot;

/// Minimum prompt-row height in pixels.
pub const ROW_MIN_HEIGHT: f32 = 28.0;
/// Height of the seam between the two prompt rows.
const ROW_SEAM_H: f32 = 1.0;
/// Extra vertical padding added to the cell height for a prompt row.
const ROW_VERTICAL_PAD: f32 = 10.0;
/// Horizontal padding within a prompt row.
const ROW_SIDE_PAD: f32 = 14.0;
/// Gap between icon and text in pixels.
const ICON_TEXT_GAP: f32 = 10.0;

/// Unicode for the circle-dot (origin) icon on the first-prompt row.
pub const ICON_FIRST: char = '⊙';
/// Unicode for the right-arrow (latest) icon on the latest-prompt row.
pub const ICON_LATEST: char = '→';
/// Unicode for the dismiss overlay icon.
pub const ICON_DISMISS: char = '×';
/// Middle-dot separator between the timer and the count in the 1-prompt state.
const SEPARATOR_GLYPH: char = '·';

/// Per-pane state the prompt bar renders from. Threaded in (rather than reading
/// a live `Pane`) so the pure model and its freeze semantics stay testable.
#[derive(Clone, Debug, Default)]
pub struct PromptBarData {
    /// Number of prompts submitted in this session (drives `#N` and row count).
    pub prompt_count: u32,
    /// Text of the first prompt in the session.
    pub first_prompt: Option<String>,
    /// Text of the most recent prompt (rendered on the second row when present).
    pub latest_prompt: Option<String>,
    /// Wall-clock instant the latest prompt was submitted (timer origin).
    pub latest_prompt_at: Option<SystemTime>,
    /// Wall-clock instant the AI finished (or last transitioned). When set, the
    /// elapsed timer freezes here instead of tracking `now`.
    pub latest_prompt_finished_at: Option<SystemTime>,
}

/// Configurable colours for the prompt bar, derived from the theme with
/// optional user overrides. Colours are sRGB `[f32; 4]`; GPUI does its own
/// linear conversion at paint time (unlike the legacy pre-multiplied renderer).
#[derive(Clone, Copy, Debug)]
pub struct PromptBarColors {
    pub first_row_bg: [f32; 4],
    pub second_row_bg: [f32; 4],
    pub text: [f32; 4],
    pub icon_first: [f32; 4],
    pub icon_latest: [f32; 4],
}

impl From<&scribe_common::theme::ChromeColors> for PromptBarColors {
    fn from(chrome: &scribe_common::theme::ChromeColors) -> Self {
        Self {
            first_row_bg: chrome.prompt_bar_first_row_bg,
            second_row_bg: chrome.prompt_bar_second_row_bg,
            text: chrome.prompt_bar_text,
            icon_first: chrome.prompt_bar_icon_first,
            icon_latest: chrome.prompt_bar_icon_latest,
        }
    }
}

impl PromptBarColors {
    /// Return this palette with `appearance.opacity` folded into the two filled
    /// row backgrounds.
    ///
    /// The prompt bar is one of the bands that tiles the window, so it has to
    /// scale with the terminal grid and the status bar or it would read as an
    /// opaque stripe across a translucent window. Text and icons keep the
    /// theme's own alpha.
    #[must_use]
    pub fn with_opacity(self, opacity: f32) -> Self {
        Self {
            first_row_bg: scale_slot(self.first_row_bg, opacity),
            second_row_bg: scale_slot(self.second_row_bg, opacity),
            ..self
        }
    }
}

/// Optional AI context-window indicator appended to the right cluster.
#[derive(Clone, Copy, Debug)]
pub struct PromptContextIndicator {
    pub percent: u8,
    pub color: [f32; 4],
}

impl PromptContextIndicator {
    /// Build the indicator for `percent`, colouring it by the configured
    /// threshold band. A malformed band hex falls back to `fallback`, so a bad
    /// config degrades the colour rather than hiding the percentage.
    #[must_use]
    pub fn from_thresholds(
        percent: u8,
        thresholds: &scribe_common::config::AiContextThresholds,
        fallback: [f32; 4],
    ) -> Self {
        let color =
            scribe_common::theme::hex_to_rgba(thresholds.color_for(percent)).unwrap_or(fallback);
        Self { percent, color }
    }
}

/// Which prompt-bar element the mouse is hovering over, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptBarHover {
    First,
    Latest,
    DismissButton,
}

/// One prompt row's icon + text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRowModel {
    pub icon: char,
    pub text: String,
}

/// Pure, display-independent prompt-bar model: the rows plus the right-cluster
/// labels (count, frozen-or-live elapsed timer, optional context meter). Built
/// without a live window and unit-tested; [`render`] lowers it onto GPUI.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptBarModel {
    pub first: PromptRowModel,
    pub latest: Option<PromptRowModel>,
    pub count_label: String,
    pub elapsed_label: Option<String>,
    pub context_label: Option<String>,
    pub context_color: Option<[f32; 4]>,
}

/// The glyph size and row cell height one pane's strip paints at.
///
/// Resolved once per frame by the view and handed to *both* [`prompt_bar_height`]
/// (the height the pane reserves before the PTY grid is sized) and [`render`]
/// (the height actually painted). Passing one value to both is what keeps the
/// reserved strip and the painted strip identical at every font size — a drift
/// there sizes the PTY grid against a band that is not there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PromptBarMetrics {
    /// Prompt text glyph size in pixels.
    pub text_size: f32,
    /// Terminal cell height the rows are sized from.
    pub cell_height: f32,
}

impl PromptBarMetrics {
    /// Resolve the strip metrics from the live grid font's glyph size and row
    /// height plus the optional `terminal.prompt_bar_font_size` override.
    ///
    /// Unset, the strip paints at the grid's own size, so an
    /// `appearance.font_size` edit or a zoom step carries the strip along.
    /// Set, the grid row height is scaled by the same ratio, keeping the row
    /// padding proportional to the text rather than frozen at the grid's.
    #[must_use]
    pub fn resolve(override_size: Option<f32>, grid_size: f32, grid_line_height: f32) -> Self {
        let text_size = override_size.unwrap_or(grid_size);
        let scale = if grid_size > 0.0 { text_size / grid_size } else { 1.0 };
        Self { text_size, cell_height: grid_line_height * scale }
    }
}

/// Row height used by the prompt-bar strip layout.
#[must_use]
pub fn prompt_bar_row_height(cell_height: f32) -> f32 {
    (cell_height + ROW_VERTICAL_PAD).max(ROW_MIN_HEIGHT)
}

/// Total prompt-bar height for `prompt_count` prompts at `metrics`.
///
/// Zero prompts (or a non-positive cell height) yields a zero-height bar; two
/// or more prompts add the second row plus its seam.
#[must_use]
pub fn prompt_bar_height(prompt_count: u32, metrics: PromptBarMetrics) -> f32 {
    if prompt_count == 0 || metrics.cell_height <= 0.0 {
        return 0.0;
    }
    let row_height = prompt_bar_row_height(metrics.cell_height);
    if prompt_count >= 2 { row_height * 2.0 + ROW_SEAM_H } else { row_height }
}

/// Format the `#N` message-count label.
#[must_use]
pub fn count_label(count: u32) -> String {
    format!("#{count}")
}

/// Format an elapsed `Duration` into the prompt-bar display string.
///
/// Thresholds:
/// - `< 60s`: `"X sec"` — counts up second-by-second from a fresh prompt.
/// - `< 1h`: `"Xm YYs"` — minutes (un-padded) and seconds (zero-padded).
/// - `>= 1h`: `"Xh YYm"` — hours (un-padded) and minutes (zero-padded).
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

/// Format the segmented context-window meter label (`▰▰▱ 66%`), clamped to 100%.
///
/// Delegates to [`scribe_common::ai_chrome::context_meter_label`] so the meter,
/// the tab suffix, and the E2E harness that asserts on them share one spelling.
#[must_use]
pub fn format_context_label(percent: u8) -> String {
    scribe_common::ai_chrome::context_meter_label(percent)
}

/// Compute elapsed `now - since`, clamped to zero when the wall clock has moved
/// backwards (DST shift, NTP correction).
fn elapsed_since(now: SystemTime, since: SystemTime) -> Duration {
    now.duration_since(since).unwrap_or(Duration::ZERO)
}

/// Formatted elapsed-time string for `data`, or `None` when there is no prompt
/// timestamp to measure from.
///
/// When `data.latest_prompt_finished_at` is `Some`, the timer is **frozen** at
/// that instant: the value reflects prompt-submission-to-finish rather than
/// wall-clock time since the prompt. Otherwise it tracks `now`.
#[must_use]
pub fn elapsed_text(data: &PromptBarData, now: SystemTime) -> Option<String> {
    let since = data.latest_prompt_at?;
    let reference = data.latest_prompt_finished_at.unwrap_or(now);
    Some(format_elapsed(elapsed_since(reference, since)))
}

/// Build the pure prompt-bar model, or `None` when nothing should render (no
/// prompts).
#[must_use]
pub fn build_model(
    data: &PromptBarData,
    now: SystemTime,
    context_indicator: Option<PromptContextIndicator>,
) -> Option<PromptBarModel> {
    if data.prompt_count == 0 {
        return None;
    }
    let first =
        PromptRowModel { icon: ICON_FIRST, text: data.first_prompt.clone().unwrap_or_default() };
    let latest = (data.prompt_count >= 2).then(|| PromptRowModel {
        icon: ICON_LATEST,
        text: data.latest_prompt.clone().unwrap_or_default(),
    });
    let (context_label, context_color) = context_indicator
        .map_or((None, None), |ind| (Some(format_context_label(ind.percent)), Some(ind.color)));
    Some(PromptBarModel {
        first,
        latest,
        count_label: count_label(data.prompt_count),
        elapsed_label: elapsed_text(data, now),
        context_label,
        context_color,
    })
}

/// Full text of the hovered prompt line (for tooltip display), or `None` for
/// the dismiss button.
#[must_use]
pub fn hovered_prompt_text(data: &PromptBarData, hover: PromptBarHover) -> Option<&str> {
    match hover {
        PromptBarHover::First => data.first_prompt.as_deref(),
        PromptBarHover::Latest => data.latest_prompt.as_deref(),
        PromptBarHover::DismissButton => None,
    }
}

/// Whether `text` would be truncated inside a prompt row `bar_width` pixels
/// wide at `cell_w` pixels per glyph. Drives the hover tooltip that reveals the
/// full prompt when it is clipped.
#[must_use]
pub fn is_prompt_truncated(text: &str, bar_width: f32, cell_w: f32) -> bool {
    let usable = bar_width - ROW_SIDE_PAD * 2.0 - cell_w - ICON_TEXT_GAP;
    let char_count = f32::from(u16::try_from(text.chars().count()).unwrap_or(u16::MAX));
    char_count * cell_w > usable
}

// ---------------------------------------------------------------------------
// GPUI rendering
// ---------------------------------------------------------------------------

fn rgba(color: [f32; 4]) -> Rgba {
    Rgba { r: color[0], g: color[1], b: color[2], a: color[3] }
}

/// Lift a colour toward white without changing its alpha (row hover tint).
fn lift(color: [f32; 4], amount: f32) -> [f32; 4] {
    let t = amount.clamp(0.0, 1.0);
    [
        color[0] + (1.0 - color[0]) * t,
        color[1] + (1.0 - color[1]) * t,
        color[2] + (1.0 - color[2]) * t,
        color[3],
    ]
}

/// Replace a colour's alpha channel.
fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

fn row_bg(base: [f32; 4], hover: Option<PromptBarHover>, target: PromptBarHover) -> [f32; 4] {
    if hover == Some(target) { lift(base, 0.035) } else { base }
}

/// Styling for one prompt row, bundled to keep [`prompt_row`]'s signature small.
#[derive(Clone, Copy)]
struct RowStyle {
    icon_color: [f32; 4],
    bg: [f32; 4],
    text_color: [f32; 4],
    height: f32,
}

fn text_span(text: impl Into<String>, color: [f32; 4]) -> gpui::Div {
    div().text_color(rgba(color)).child(text.into())
}

/// Build a single prompt row: icon, truncating prompt text, and (optionally) the
/// right-edge cluster placed after a flexible spacer.
fn prompt_row(
    row: &PromptRowModel,
    style: RowStyle,
    right: Option<gpui::AnyElement>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(style.height))
        .px(px(ROW_SIDE_PAD))
        .bg(rgba(style.bg))
        .child(text_span(row.icon.to_string(), style.icon_color).flex_none())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .ml(px(ICON_TEXT_GAP))
                .truncate()
                .text_color(rgba(style.text_color))
                .child(row.text.clone()),
        )
        .children(right)
}

/// Right-edge cluster of the elapsed timer alone (row 1 in the two-prompt state).
fn timer_cluster(model: &PromptBarModel, colors: &PromptBarColors) -> Option<gpui::AnyElement> {
    let elapsed = model.elapsed_label.as_ref()?;
    Some(
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .child(text_span(elapsed.clone(), with_alpha(colors.text, 0.42)))
            .into_any_element(),
    )
}

/// Right-edge cluster of the `#N` count plus the optional context meter. In the
/// one-prompt state a middle-dot separator joins the timer that precedes it.
fn count_cluster(
    model: &PromptBarModel,
    colors: &PromptBarColors,
    lead_separator: bool,
) -> gpui::Div {
    let count_color = with_alpha(colors.text, 0.62);
    let separator_color = with_alpha(colors.text, 0.28);
    let mut c = div().flex().flex_row().items_center().flex_none().gap(px(6.0));
    if lead_separator {
        c = c.child(text_span(SEPARATOR_GLYPH.to_string(), separator_color));
    }
    c = c.child(text_span(model.count_label.clone(), count_color));
    if let (Some(label), Some(color)) = (&model.context_label, model.context_color) {
        c = c
            .child(text_span(SEPARATOR_GLYPH.to_string(), separator_color))
            .child(text_span(label.clone(), color));
    }
    c
}

/// Render the prompt-bar strip for a pane.
///
/// `metrics` sizes both the rows and the text — the same value the pane
/// reserved space with via [`prompt_bar_height`]; `hover` tints the hovered row
/// and reveals the dismiss affordance. Interaction (click/hover wiring) is
/// attached by the view; this function owns only the visual lowering of
/// [`PromptBarModel`].
pub fn render(
    model: &PromptBarModel,
    colors: &PromptBarColors,
    metrics: PromptBarMetrics,
    hover: Option<PromptBarHover>,
) -> impl IntoElement {
    let row_h = prompt_bar_row_height(metrics.cell_height);
    // `flex_none` for the same reason the status bar carries it: the strip is a
    // fixed-height band stacked under the flex-grown terminal grid, and a
    // shrinkable band would be squeezed away rather than clipping the grid.
    let prompt_state = if model.latest.is_some() {
        format!("AI prompt status: latest prompt {} received", model.count_label)
    } else {
        format!("AI prompt status: prompt {} received", model.count_label)
    };
    let mut strip = div()
        .id("ai-prompt-status")
        .role(Role::Status)
        .aria_label(prompt_state)
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .font_family("monospace")
        .text_size(px(metrics.text_size))
        .relative();

    let first_style = RowStyle {
        icon_color: colors.icon_first,
        bg: row_bg(colors.first_row_bg, hover, PromptBarHover::First),
        text_color: colors.text,
        height: row_h,
    };

    if let Some(latest) = &model.latest {
        // Two-prompt state: timer on row 1, count + context drop to row 2.
        strip = strip
            .child(prompt_row(&model.first, first_style, timer_cluster(model, colors)))
            .child(div().w_full().h(px(ROW_SEAM_H)).bg(rgba(with_alpha(colors.text, 0.12))))
            .child(prompt_row(
                latest,
                RowStyle {
                    icon_color: colors.icon_latest,
                    bg: row_bg(colors.second_row_bg, hover, PromptBarHover::Latest),
                    text_color: colors.text,
                    height: row_h,
                },
                Some(count_cluster(model, colors, false).into_any_element()),
            ));
    } else {
        // One-prompt state: `<timer> · #N (· meter)` all on row 1.
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(6.0))
            .children(timer_cluster(model, colors))
            .child(count_cluster(model, colors, model.elapsed_label.is_some()));
        strip = strip.child(prompt_row(&model.first, first_style, Some(right.into_any_element())));
    }

    // Dismiss affordance: shown in the left padding lane while the bar is
    // hovered, mirroring the legacy overlay.
    if hover.is_some() {
        strip = strip.child(
            div()
                .absolute()
                .left(px(1.0))
                .top(px(0.0))
                .h(px(row_h))
                .flex()
                .items_center()
                .child(text_span(ICON_DISMISS.to_string(), with_alpha(colors.text, 0.94))),
        );
    }

    strip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed formats span sec, minute, and hour bands]]
    #[gpui::test]
    fn format_elapsed_covers_all_bands() {
        assert_eq!(format_elapsed(d(0)), "0 sec");
        assert_eq!(format_elapsed(d(59)), "59 sec");
        assert_eq!(format_elapsed(d(60)), "1m 00s");
        assert_eq!(format_elapsed(d(12 * 60 + 4)), "12m 04s");
        assert_eq!(format_elapsed(d(59 * 60 + 59)), "59m 59s");
        assert_eq!(format_elapsed(d(3600)), "1h 00m");
        assert_eq!(format_elapsed(d(18 * 3600 + 3 * 60)), "18h 03m");
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed timer tracks now until the AI stops]]
    #[gpui::test]
    fn elapsed_timer_tracks_now_when_running() {
        let data = PromptBarData {
            prompt_count: 1,
            first_prompt: Some("build the thing".into()),
            latest_prompt_at: Some(at(100)),
            latest_prompt_finished_at: None,
            ..PromptBarData::default()
        };
        // now is 130s after submission → live 30s.
        assert_eq!(elapsed_text(&data, at(130)).as_deref(), Some("30 sec"));
        // A later now keeps advancing while unfinished.
        assert_eq!(elapsed_text(&data, at(160)).as_deref(), Some("1m 00s"));
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed timer freezes when the AI stops]]
    #[gpui::test]
    fn elapsed_timer_freezes_on_finish() {
        let data = PromptBarData {
            prompt_count: 1,
            first_prompt: Some("build the thing".into()),
            latest_prompt_at: Some(at(100)),
            // AI finished 45s after submission.
            latest_prompt_finished_at: Some(at(145)),
            ..PromptBarData::default()
        };
        // Regardless of how far `now` advances, the frozen value holds at 45s.
        assert_eq!(elapsed_text(&data, at(200)).as_deref(), Some("45 sec"));
        assert_eq!(elapsed_text(&data, at(100_000)).as_deref(), Some("45 sec"));
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed clamps a backwards wall clock]]
    #[gpui::test]
    fn elapsed_clamps_clock_skew() {
        let data = PromptBarData {
            prompt_count: 1,
            latest_prompt_at: Some(at(200)),
            latest_prompt_finished_at: None,
            ..PromptBarData::default()
        };
        // now is before submission → clamp to zero, not a panic or underflow.
        assert_eq!(elapsed_text(&data, at(100)).as_deref(), Some("0 sec"));
    }

    // @lat: [[client#GPUI Prompt Bar#No timer without a prompt timestamp]]
    #[gpui::test]
    fn elapsed_none_without_timestamp() {
        let data = PromptBarData { prompt_count: 1, ..PromptBarData::default() };
        assert_eq!(elapsed_text(&data, at(100)), None);
    }

    // @lat: [[client#GPUI Prompt Bar#Context meter fills and clamps]]
    #[gpui::test]
    fn context_label_fills_and_clamps() {
        assert_eq!(format_context_label(0), "▱▱▱ 0%");
        assert_eq!(format_context_label(1), "▰▱▱ 1%");
        assert_eq!(format_context_label(34), "▰▰▱ 34%");
        assert_eq!(format_context_label(70), "▰▰▰ 70%");
        assert_eq!(format_context_label(100), "▰▰▰ 100%");
        assert_eq!(format_context_label(u8::MAX), "▰▰▰ 100%");
    }

    // @lat: [[client#GPUI Prompt Bar#Strip height tracks the prompt count]]
    #[gpui::test]
    fn height_tracks_prompt_count() {
        // Compare by bit pattern: these are exact f32 arithmetic results, so the
        // strict `clippy::float_cmp` lint stays satisfied without approximation.
        let eq = |a: f32, b: f32| a.to_bits() == b.to_bits();
        let m = |cell_height| PromptBarMetrics { text_size: 14.0, cell_height };
        assert!(eq(prompt_bar_height(0, m(16.0)), 0.0));
        assert!(eq(prompt_bar_height(1, m(16.0)), prompt_bar_row_height(16.0)));
        assert!(eq(prompt_bar_height(2, m(16.0)), prompt_bar_row_height(16.0) * 2.0 + ROW_SEAM_H));
        assert!(eq(prompt_bar_height(1, m(0.0)), 0.0), "non-positive cell height yields no bar");
    }

    // @lat: [[client#GPUI Prompt Bar#Strip metrics follow the grid font]]
    #[gpui::test]
    fn metrics_follow_grid_font_unless_overridden() {
        let eq = |a: f32, b: f32| a.to_bits() == b.to_bits();
        let (size, line_height) = (20.0, 26.0);

        let followed = PromptBarMetrics::resolve(None, size, line_height);
        assert!(eq(followed.text_size, size), "unset override paints at the grid size");
        assert!(eq(followed.cell_height, line_height), "and reserves the grid's row");

        let overridden = PromptBarMetrics::resolve(Some(10.0), size, line_height);
        assert!(eq(overridden.text_size, 10.0), "an explicit size wins over the grid");
        assert!(
            eq(overridden.cell_height, line_height * 0.5),
            "and scales the row height by the same ratio so padding stays proportional"
        );
    }

    // @lat: [[client#GPUI Prompt Bar#Model shows one row for one prompt, two for many]]
    #[gpui::test]
    fn model_row_count_follows_prompt_count() {
        let one = PromptBarData {
            prompt_count: 1,
            first_prompt: Some("first".into()),
            ..PromptBarData::default()
        };
        let model = build_model(&one, at(0), None).expect("one prompt renders");
        assert!(model.latest.is_none(), "a single prompt has no latest row");
        assert_eq!(model.count_label, "#1");

        let two = PromptBarData {
            prompt_count: 3,
            first_prompt: Some("first".into()),
            latest_prompt: Some("latest".into()),
            ..PromptBarData::default()
        };
        let multi = build_model(&two, at(0), None).expect("multi prompt renders");
        assert_eq!(multi.latest.as_ref().unwrap().icon, ICON_LATEST);
        assert_eq!(multi.count_label, "#3");

        assert!(build_model(&PromptBarData::default(), at(0), None).is_none());
    }

    // @lat: [[client#GPUI Prompt Bar#Truncation predicate gates the hover tooltip]]
    #[gpui::test]
    fn truncation_predicate_flags_overflow() {
        let cell_w = 8.0;
        // A wide bar fits a short prompt.
        assert!(!is_prompt_truncated("hi", 400.0, cell_w));
        // A narrow bar cannot fit a long prompt.
        assert!(is_prompt_truncated(&"x".repeat(80), 120.0, cell_w));
    }
}
