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

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gpui::{App, ElementId, Rgba, Role, Window, div, prelude::*, px};
use scribe_common::protocol::{SessionPromptState, from_epoch_secs};

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
    /// Prompt history for the session — the same record the server retains and
    /// the restore snapshot persists, rather than a third declaration of the
    /// same five fields.
    pub prompts: SessionPromptState,
    /// Set by the strip's `×` overlay: the pane keeps its prompt history but
    /// paints no bar and reserves no rows until the record is dropped.
    pub dismissed: bool,
}

impl From<SessionPromptState> for PromptBarData {
    /// Adopt the prompt history the server retained for a session.
    ///
    /// `dismissed` has no wire counterpart on purpose: dismissal is a local
    /// gesture against a pane, so a reattaching client starts with the bar
    /// shown, exactly as a fresh window would.
    fn from(prompts: SessionPromptState) -> Self {
        Self { prompts, dismissed: false }
    }
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

/// Which prompt row the mouse is hovering over, if any.
///
/// The `×` overlay is not a target of its own: it lies inside the first row's
/// hitbox, so pointing at it keeps that row hovered and the overlay visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptBarHover {
    First,
    Latest,
}

/// One prompt row's icon + text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRowModel {
    pub icon: char,
    /// Single logical line painted inside the fixed-height row.
    pub text: String,
    /// Unprojected prompt revealed by the hover tooltip.
    pub full_text: String,
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
    /// Per-glyph advance width the strip's monospace text lays out at, which
    /// is what [`is_prompt_truncated`] measures a row against.
    pub cell_width: f32,
}

impl PromptBarMetrics {
    /// Resolve the strip metrics from the live grid font's glyph size and row
    /// height plus the optional `terminal.prompt_bar_font_size` override.
    ///
    /// Unset, the strip paints at the grid's own size, so an
    /// `appearance.font_size` edit or a zoom step carries the strip along.
    /// Set, the grid row height and advance width are scaled by the same ratio,
    /// keeping the row padding and the truncation measure proportional to the
    /// text rather than frozen at the grid's.
    #[must_use]
    pub fn resolve(
        override_size: Option<f32>,
        grid_size: f32,
        grid_line_height: f32,
        grid_cell_width: f32,
    ) -> Self {
        let text_size = override_size.unwrap_or(grid_size);
        let scale = if grid_size > 0.0 { text_size / grid_size } else { 1.0 };
        Self {
            text_size,
            cell_height: grid_line_height * scale,
            cell_width: grid_cell_width * scale,
        }
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
    let since = from_epoch_secs(data.prompts.latest_prompt_at)?;
    let reference = from_epoch_secs(data.prompts.latest_prompt_finished_at).unwrap_or(now);
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
    if data.prompts.prompt_count == 0 {
        return None;
    }
    let first = prompt_row_model(ICON_FIRST, data.prompts.first_prompt.as_deref());
    let latest = (data.prompts.prompt_count >= 2)
        .then(|| prompt_row_model(ICON_LATEST, data.prompts.latest_prompt.as_deref()));
    let (context_label, context_color) = context_indicator
        .map_or((None, None), |ind| (Some(format_context_label(ind.percent)), Some(ind.color)));
    Some(PromptBarModel {
        first,
        latest,
        count_label: count_label(data.prompts.prompt_count),
        elapsed_label: elapsed_text(data, now),
        context_label,
        context_color,
    })
}

fn first_logical_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn prompt_row_model(icon: char, full_text: Option<&str>) -> PromptRowModel {
    let full_text = full_text.unwrap_or_default();
    PromptRowModel {
        icon,
        text: first_logical_line(full_text).to_owned(),
        full_text: full_text.to_owned(),
    }
}

/// Whether `text` would be truncated inside a prompt row `bar_width` pixels
/// wide at `cell_w` pixels per glyph. Drives the hover tooltip that reveals the
/// full prompt when it is clipped.
#[must_use]
pub fn is_prompt_truncated(text: &str, bar_width: f32, cell_w: f32) -> bool {
    let display_text = first_logical_line(text);
    if display_text.len() != text.len() {
        return true;
    }
    let usable = bar_width - ROW_SIDE_PAD * 2.0 - cell_w - ICON_TEXT_GAP;
    let char_count = f32::from(u16::try_from(display_text.chars().count()).unwrap_or(u16::MAX));
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

/// Called when the pointer enters (`true`) or leaves (`false`) a hover target.
/// The target is named on both edges so a leave can be matched against the
/// hover the view actually holds.
pub type PromptHoverHandler = Rc<dyn Fn(PromptBarHover, bool, &mut Window, &mut App)>;
/// Called when the left-edge `×` overlay is clicked.
pub type PromptDismissHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// Interactive wiring for one pane's strip, supplied by the view.
///
/// The same split [`crate::status_bar::StatusBarActions`] uses: this module
/// owns the visual lowering and the view owns the state, so the hovered target
/// is tracked there (keyed by session, so a split window tints exactly one
/// strip) and handed back in through `hover`.
pub struct PromptBarActions {
    /// Element id for this pane's strip. Every interactive child is scoped
    /// under it, so two panes' strips never share GPUI's hover or tooltip
    /// state.
    pub id: ElementId,
    /// Which target the pointer is over, per the view.
    pub hover: Option<PromptBarHover>,
    /// Painted strip width in pixels, which [`is_prompt_truncated`] needs to
    /// tell a clipped row (tooltip warranted) from one that already fits.
    pub width: f32,
    pub on_hover: PromptHoverHandler,
    pub on_dismiss: PromptDismissHandler,
}

/// Styling for one prompt row, bundled to keep [`prompt_row`]'s signature small.
#[derive(Clone, Copy)]
struct RowStyle {
    icon_color: [f32; 4],
    bg: [f32; 4],
    text_color: [f32; 4],
    height: f32,
}

/// Interaction for one prompt row: its hover target and whether its text is
/// clipped hard enough to earn the reveal-on-hover tooltip.
struct RowWiring {
    id: &'static str,
    target: PromptBarHover,
    /// `true` when the row's text does not fit, so hovering should reveal it.
    truncated: bool,
    text_size: f32,
    on_hover: PromptHoverHandler,
}

/// The hover reveal for a clipped prompt row: the row's full text, wrapped.
///
/// Painted opaque over whatever is behind it — the palette handed to
/// [`render`] already carries `appearance.opacity`, and a see-through popup
/// over terminal output is unreadable.
struct PromptTooltip {
    text: String,
    style: RowStyle,
    text_size: f32,
}

impl gpui::Render for PromptTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .max_w(px(560.0))
            .bg(rgba(with_alpha(self.style.bg, 1.0)))
            .border_1()
            .border_color(rgba(with_alpha(self.style.text_color, 0.28)))
            .font_family("monospace")
            .text_size(px(self.text_size))
            .text_color(rgba(self.style.text_color))
            .child(self.text.clone())
    }
}

fn text_span(text: impl Into<String>, color: [f32; 4]) -> gpui::Div {
    div().text_color(rgba(color)).child(text.into())
}

/// Build a single prompt row: icon, truncating prompt text, and (optionally) the
/// right-edge cluster placed after a flexible spacer.
///
/// The row is the hover target: entering it reports `wiring.target` to the view
/// (which tints it back through [`row_bg`] and un-hides the dismiss overlay),
/// and a clipped row also carries a zero-delay tooltip with its full text.
/// The delay override is the point of the tooltip here — the reveal has to read
/// as "the row expanded", not as a hint that shows up half a second later.
fn prompt_row(
    row: &PromptRowModel,
    style: RowStyle,
    wiring: &RowWiring,
    right: Option<gpui::AnyElement>,
) -> impl IntoElement {
    let target = wiring.target;
    let on_hover = Rc::clone(&wiring.on_hover);
    let body = div()
        .id(wiring.id)
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
                .h_full()
                .flex()
                .items_center()
                .ml(px(ICON_TEXT_GAP))
                .truncate()
                .text_color(rgba(style.text_color))
                .child(row.text.clone()),
        )
        .children(right)
        .on_hover(move |hovered: &bool, window, cx| on_hover(target, *hovered, window, cx));
    if !wiring.truncated {
        return body;
    }
    let text = row.full_text.clone();
    let text_size = wiring.text_size;
    body.tooltip(move |_window, cx| {
        cx.new(|_| PromptTooltip { text: text.clone(), style, text_size }).into()
    })
    .tooltip_show_delay(Duration::ZERO)
}

/// The `×` overlay that hides the bar, laid into row 1's left padding lane.
///
/// Only built while the strip is hovered, mirroring the legacy overlay. It
/// sits inside row 1's own hitbox, so pointing at it keeps that row hovered
/// and the overlay therefore stays up long enough to be clicked.
fn dismiss_overlay(
    row_height: f32,
    colors: &PromptBarColors,
    on_dismiss: PromptDismissHandler,
) -> impl IntoElement {
    div()
        .id("prompt-dismiss")
        .absolute()
        .left(px(1.0))
        .top(px(0.0))
        .h(px(row_height))
        .w(px(ROW_SIDE_PAD))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_click(move |_, window, cx| on_dismiss(window, cx))
        .child(text_span(ICON_DISMISS.to_string(), with_alpha(colors.text, 0.94)))
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
/// reserved space with via [`prompt_bar_height`]; `actions` carries the view's
/// hover state in (tinting that row and revealing the dismiss affordance) and
/// the listeners that keep it current back out.
pub fn render(
    model: &PromptBarModel,
    colors: &PromptBarColors,
    metrics: PromptBarMetrics,
    actions: PromptBarActions,
) -> impl IntoElement {
    let PromptBarActions { id: strip_id, hover, width, on_hover, on_dismiss } = actions;
    let row_h = prompt_bar_row_height(metrics.cell_height);
    let wiring = |row_id, target, text: &str| RowWiring {
        id: row_id,
        target,
        truncated: is_prompt_truncated(text, width, metrics.cell_width),
        text_size: metrics.text_size,
        on_hover: Rc::clone(&on_hover),
    };
    // `flex_none` for the same reason the status bar carries it: the strip is a
    // fixed-height band stacked under the flex-grown terminal grid, and a
    // shrinkable band would be squeezed away rather than clipping the grid.
    let prompt_state = if model.latest.is_some() {
        format!("AI prompt status: latest prompt {} received", model.count_label)
    } else {
        format!("AI prompt status: prompt {} received", model.count_label)
    };
    let mut strip = div()
        .id(strip_id)
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

    let first_wiring = wiring("prompt-row-first", PromptBarHover::First, &model.first.full_text);

    if let Some(latest) = &model.latest {
        // Two-prompt state: timer on row 1, count + context drop to row 2.
        strip = strip
            .child(prompt_row(
                &model.first,
                first_style,
                &first_wiring,
                timer_cluster(model, colors),
            ))
            .child(div().w_full().h(px(ROW_SEAM_H)).bg(rgba(with_alpha(colors.text, 0.12))))
            .child(prompt_row(
                latest,
                RowStyle {
                    icon_color: colors.icon_latest,
                    bg: row_bg(colors.second_row_bg, hover, PromptBarHover::Latest),
                    text_color: colors.text,
                    height: row_h,
                },
                &wiring("prompt-row-latest", PromptBarHover::Latest, &latest.full_text),
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
        strip = strip.child(prompt_row(
            &model.first,
            first_style,
            &first_wiring,
            Some(right.into_any_element()),
        ));
    }

    if hover.is_some() {
        strip = strip.child(dismiss_overlay(row_h, colors, on_dismiss));
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
        let data = PromptBarData::from(SessionPromptState {
            prompt_count: 1,
            first_prompt: Some("build the thing".into()),
            latest_prompt_at: Some(100),
            latest_prompt_finished_at: None,
            ..SessionPromptState::default()
        });
        // now is 130s after submission → live 30s.
        assert_eq!(elapsed_text(&data, at(130)).as_deref(), Some("30 sec"));
        // A later now keeps advancing while unfinished.
        assert_eq!(elapsed_text(&data, at(160)).as_deref(), Some("1m 00s"));
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed timer freezes when the AI stops]]
    #[gpui::test]
    fn elapsed_timer_freezes_on_finish() {
        let data = PromptBarData::from(SessionPromptState {
            prompt_count: 1,
            first_prompt: Some("build the thing".into()),
            latest_prompt_at: Some(100),
            // AI finished 45s after submission.
            latest_prompt_finished_at: Some(145),
            ..SessionPromptState::default()
        });
        // Regardless of how far `now` advances, the frozen value holds at 45s.
        assert_eq!(elapsed_text(&data, at(200)).as_deref(), Some("45 sec"));
        assert_eq!(elapsed_text(&data, at(100_000)).as_deref(), Some("45 sec"));
    }

    // @lat: [[client#GPUI Prompt Bar#Elapsed clamps a backwards wall clock]]
    #[gpui::test]
    fn elapsed_clamps_clock_skew() {
        let data = PromptBarData::from(SessionPromptState {
            prompt_count: 1,
            latest_prompt_at: Some(200),
            latest_prompt_finished_at: None,
            ..SessionPromptState::default()
        });
        // now is before submission → clamp to zero, not a panic or underflow.
        assert_eq!(elapsed_text(&data, at(100)).as_deref(), Some("0 sec"));
    }

    // @lat: [[client#GPUI Prompt Bar#No timer without a prompt timestamp]]
    #[gpui::test]
    fn elapsed_none_without_timestamp() {
        let data = PromptBarData::from(SessionPromptState {
            prompt_count: 1,
            ..SessionPromptState::default()
        });
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
        let m = |cell_height| PromptBarMetrics { text_size: 14.0, cell_height, cell_width: 8.0 };
        assert!(eq(prompt_bar_height(0, m(16.0)), 0.0));
        assert!(eq(prompt_bar_height(1, m(16.0)), prompt_bar_row_height(16.0)));
        assert!(eq(prompt_bar_height(2, m(16.0)), prompt_bar_row_height(16.0) * 2.0 + ROW_SEAM_H));
        assert!(eq(prompt_bar_height(1, m(0.0)), 0.0), "non-positive cell height yields no bar");
    }

    // @lat: [[client#GPUI Prompt Bar#Strip metrics follow the grid font]]
    #[gpui::test]
    fn metrics_follow_grid_font_unless_overridden() {
        let eq = |a: f32, b: f32| a.to_bits() == b.to_bits();
        let (size, line_height, cell_width) = (20.0, 26.0, 12.0);

        let followed = PromptBarMetrics::resolve(None, size, line_height, cell_width);
        assert!(eq(followed.text_size, size), "unset override paints at the grid size");
        assert!(eq(followed.cell_height, line_height), "and reserves the grid's row");
        assert!(eq(followed.cell_width, cell_width), "and measures with the grid's advance");

        let overridden = PromptBarMetrics::resolve(Some(10.0), size, line_height, cell_width);
        assert!(eq(overridden.text_size, 10.0), "an explicit size wins over the grid");
        assert!(
            eq(overridden.cell_height, line_height * 0.5),
            "and scales the row height by the same ratio so padding stays proportional"
        );
        assert!(
            eq(overridden.cell_width, cell_width * 0.5),
            "and scales the advance width too, so truncation is measured at the painted size"
        );
    }

    // @lat: [[client#GPUI Prompt Bar#Model shows one row for one prompt, two for many]]
    #[gpui::test]
    fn model_row_count_follows_prompt_count() {
        let one = PromptBarData::from(SessionPromptState {
            prompt_count: 1,
            first_prompt: Some("first".into()),
            ..SessionPromptState::default()
        });
        let model = build_model(&one, at(0), None).expect("one prompt renders");
        assert!(model.latest.is_none(), "a single prompt has no latest row");
        assert_eq!(model.count_label, "#1");

        let two = PromptBarData::from(SessionPromptState {
            prompt_count: 3,
            first_prompt: Some("first".into()),
            latest_prompt: Some("latest".into()),
            ..SessionPromptState::default()
        });
        let multi = build_model(&two, at(0), None).expect("multi prompt renders");
        assert_eq!(multi.latest.as_ref().unwrap().icon, ICON_LATEST);
        assert_eq!(multi.count_label, "#3");

        assert!(build_model(&PromptBarData::default(), at(0), None).is_none());
    }

    // @lat: [[client#GPUI Prompt Bar#Multiline prompts paint one logical line and retain full text]]
    #[gpui::test]
    fn model_projects_first_logical_line_for_both_rows() {
        let first = "google_cloud_run_v2_service.app\nlocations/us-west1/services/cue-server";
        let latest = "short\r\nsecond line";
        let data = PromptBarData::from(SessionPromptState {
            prompt_count: 2,
            first_prompt: Some(first.into()),
            latest_prompt: Some(latest.into()),
            ..SessionPromptState::default()
        });

        let model = build_model(&data, at(0), None).expect("two prompts render");
        assert_eq!(model.first.text, "google_cloud_run_v2_service.app");
        assert_eq!(model.first.full_text, first);
        let latest_row = model.latest.unwrap();
        assert_eq!(latest_row.text, "short");
        assert_eq!(latest_row.full_text, latest);
        assert_eq!(data.prompts.first_prompt.as_deref(), Some(first));
        assert_eq!(data.prompts.latest_prompt.as_deref(), Some(latest));
    }

    // @lat: [[client#GPUI Prompt Bar#Truncation predicate gates the hover tooltip]]
    #[gpui::test]
    fn truncation_predicate_flags_overflow() {
        let cell_w = 8.0;
        // A wide bar fits a short prompt.
        assert!(!is_prompt_truncated("hi", 400.0, cell_w));
        // A narrow bar cannot fit a long prompt.
        assert!(is_prompt_truncated(&"x".repeat(80), 120.0, cell_w));
        // An omitted logical-line suffix earns a tooltip even when the first
        // line fits with room to spare.
        assert!(is_prompt_truncated("short\r\nsecond line", 10_000.0, cell_w));
    }
}
