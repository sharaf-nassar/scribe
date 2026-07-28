//! Hover tooltip overlay for the GPUI client rebuild.
//!
//! The winit client painted tooltips as GPU quads (a border quad, a
//! background quad, then glyphs) centred on an anchor rect and clamped to the
//! viewport, plus a dedicated OSC 8 hover-tooltip that head+tail-truncated a long
//! URI so it fit the box. This port keeps that geometry as pure, testable
//! functions — [`clamp_tooltip_x`] (centre-on-anchor, clamp to the viewport) and
//! [`truncate_url`] (the `head…tail` URL elision) — and lowers the paint onto a
//! GPUI [`tooltip_element`] with rounded corners and a drop shadow instead of the
//! hand-placed quads.

use gpui::{AnyElement, Rgba, div, prelude::*, px};
use scribe_common::theme::ChromeColors;

use crate::layout::Rect;
use crate::tab_bar::srgba;

/// Whether the tooltip should appear above or below its anchor rect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TooltipPosition {
    /// Draw the tooltip immediately above the anchor's top edge.
    Above,
    /// Draw the tooltip immediately below the anchor's bottom edge.
    Below,
}

/// A hover target that can show a tooltip: the display text plus the anchor
/// rectangle it is positioned against.
#[derive(Clone, Debug)]
pub struct TooltipAnchor {
    /// Text shown inside the tooltip box.
    pub text: String,
    /// The on-screen rectangle the tooltip is centred over.
    pub rect: Rect,
}

/// One display-character advance, matching the fixed-width terminal cell metric.
/// The tooltip box reserves one character of padding on each side of the text,
/// mirroring the winit renderer's leading/trailing space glyphs.
pub const TOOLTIP_PAD_COLS: usize = 2;

/// Compute the tooltip box width in pixels for `text` at `char_width` per
/// character, including one character of padding on each side.
#[must_use]
pub fn tooltip_width(text: &str, char_width: f32) -> f32 {
    let cols = text.chars().count() + TOOLTIP_PAD_COLS;
    u16::try_from(cols).map_or(f32::from(u16::MAX), f32::from) * char_width
}

/// Centre a `tooltip_width`-wide box horizontally over `anchor` and clamp it to
/// stay within `[0, viewport_width]`. Ported verbatim from the winit tooltip
/// renderer so a tooltip anchored near the window edge slides inward instead of
/// clipping. When the tooltip is wider than the viewport the clamp collapses to
/// the left edge (`0.0`).
#[must_use]
pub fn clamp_tooltip_x(anchor: Rect, tooltip_width: f32, viewport_width: f32) -> f32 {
    let center_x = anchor.x + anchor.width / 2.0;
    (center_x - tooltip_width / 2.0).clamp(0.0, (viewport_width - tooltip_width).max(0.0))
}

/// Top-left `y` of a `tooltip_height`-tall box for the chosen [`TooltipPosition`]
/// relative to `anchor`.
#[must_use]
pub fn tooltip_y(anchor: Rect, tooltip_height: f32, position: TooltipPosition) -> f32 {
    match position {
        TooltipPosition::Above => anchor.y - tooltip_height,
        TooltipPosition::Below => anchor.y + anchor.height,
    }
}

/// Head+tail-truncate `uri` to at most `max_cols` display columns, inserting an
/// `...` ellipsis in the middle so both the scheme/host head and the path tail
/// stay visible. Ported verbatim from the winit client's `osc8_tooltip_truncate`
/// (spec 009 FR-006): URIs at or under the budget are returned unchanged, a
/// budget of three columns or fewer falls back to a plain head cut, and the
/// remaining budget is split head-heavy (`div_ceil`) so an odd column favours the
/// head. Char-based so a multibyte URI never splits mid-codepoint.
#[must_use]
pub fn truncate_url(uri: &str, max_cols: usize) -> String {
    let chars: Vec<char> = uri.chars().collect();
    if chars.len() <= max_cols {
        return uri.to_owned();
    }
    if max_cols <= 3 {
        return chars.into_iter().take(max_cols).collect();
    }
    let budget = max_cols.saturating_sub(3);
    let head_chars = budget.div_ceil(2);
    let tail_chars = budget - head_chars;
    let mut out: String = chars.iter().take(head_chars).collect();
    out.push_str("...");
    out.extend(chars.iter().skip(chars.len() - tail_chars));
    out
}

/// Resolved GPUI colours for the tooltip box, derived from the theme chrome.
#[derive(Clone, Copy)]
pub struct TooltipColors {
    /// Box background.
    pub bg: Rgba,
    /// Text colour.
    pub fg: Rgba,
    /// 1px border colour.
    pub border: Rgba,
}

impl From<&ChromeColors> for TooltipColors {
    fn from(chrome: &ChromeColors) -> Self {
        let mut bg = srgba(chrome.tab_bar_bg);
        bg.a = 0.98;
        Self {
            bg,
            fg: srgba(chrome.tab_text_active),
            border: with_alpha(srgba(chrome.tab_text), 0.25),
        }
    }
}

/// Return `color` with a replaced alpha channel.
fn with_alpha(color: Rgba, alpha: f32) -> Rgba {
    Rgba { a: alpha, ..color }
}

/// Inputs for [`tooltip_element`]: the text, the anchor geometry, the theme
/// colours, and the fixed-width cell metric the box is sized against.
pub struct TooltipRender<'a> {
    /// Text shown inside the box.
    pub text: &'a str,
    /// The on-screen rectangle the tooltip is centred over.
    pub anchor: Rect,
    /// Whether the box sits above or below the anchor.
    pub position: TooltipPosition,
    /// Window width used to clamp the box inside the viewport.
    pub viewport_width: f32,
    /// Resolved box colours.
    pub colors: &'a TooltipColors,
    /// Advance width of one display character.
    pub char_width: f32,
    /// Line height (box height).
    pub line_height: f32,
}

/// Build the absolutely-positioned tooltip box for the given [`TooltipRender`].
///
/// The returned element is `absolute`-positioned at the clamped `(x, y)` inside a
/// `relative` overlay layer, with rounded corners, a drop shadow, a 1px border,
/// and single-character horizontal padding, replacing the winit border/background
/// quad pair. `char_width` and `line_height` size the box to the fixed-width cell
/// metric so the tooltip lines up with the terminal grid.
#[must_use]
pub fn tooltip_element(params: &TooltipRender<'_>) -> AnyElement {
    let &TooltipRender { text, anchor, position, viewport_width, colors, char_width, line_height } =
        params;
    let width = tooltip_width(text, char_width);
    let x = clamp_tooltip_x(anchor, width, viewport_width);
    let y = tooltip_y(anchor, line_height, position);
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(width))
        .h(px(line_height))
        .flex()
        .items_center()
        .justify_center()
        .bg(colors.bg)
        .text_color(colors.fg)
        .text_xs()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .shadow_md()
        .child(text.to_owned())
        .into_any_element()
}

#[cfg(test)]
mod tests;
