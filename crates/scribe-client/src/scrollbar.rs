//! Overlay scrollbar with command-status marks: state, geometry, and hit-testing.
//!
//! The scrollbar is a non-reserving overlay on the right edge of each pane's
//! content area — a bespoke Scribe differentiator with no Zed equivalent, so it
//! is rebuilt here as pure, renderer-independent logic rather than cribbed. It
//! fades in on scroll, fades out after inactivity, widens on hover, and supports
//! click-to-jump and drag-to-scroll. Command boundaries render as coloured tick
//! marks anchored to absolute scrollback rows, so a trim that drops the oldest
//! rows shifts every surviving mark (the binary's `session_lifecycle`
//! prompt-mark store owns that shift and hands the ticks in as [`CommandMark`]).
//!
//! Ported byte-for-byte from the legacy winit client's `scrollbar.rs`. The pure
//! logic here — [`ScrollbarState`] fade/width animation, [`compute_thumb`]
//! geometry, hit-testing, and [`build_scrollbar_render`] thumb/tick emission —
//! stays free of GPUI types so it is exercised by `#[gpui::test]`; the terminal
//! element lowers [`ScrollbarRender`] onto GPUI quads on the live paint pass.
//!
//! Unlike the legacy renderer, the geometry constants here are fixed rather
//! than config-driven: `appearance.scrollbar_width` and
//! `appearance.scrollbar_color` are declared removed keys for the GPUI client
//! (they were bespoke-pipeline hover-lerp inputs), so the width comes from
//! [`SCROLLBAR_WIDTH`] and the colour from the theme's derived
//! `chrome.scrollbar` slot via [`ScrollbarStyle::from_theme`].

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use scribe_common::theme::Theme;

use crate::layout::Rect;

/// Shared handle to one pane's scrollbar state.
///
/// The view owns the state across frames (the fade is a wall-clock animation,
/// not a per-frame derivation) while the paint pass has to mutate it — the
/// paint pass is what folds the hover width target in, and it is the only
/// place the pane's real pixel rect is known. Both ends live on the GPUI
/// thread, so a `RefCell` is the whole synchronisation story.
pub type ScrollbarHandle = Rc<RefCell<ScrollbarState>>;

/// Resolved command outcome for a scrollbar tick.
///
/// The tick is a redundant secondary cue — the always-visible status-bar glyph
/// is the authoritative accessible signal — so `Unknown` keeps the existing
/// neutral tick colour and only `Success`/`Failure` pull theme-derived hues.
/// `Unknown` is also the resting state of a command whose shell reported no
/// exit code (FR-012): an unreported exit is never rendered as a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// Command outcome is not yet known (still running or unreported).
    Unknown,
    /// Command exited successfully.
    Success,
    /// Command exited with a failure status.
    Failure,
}

/// A command boundary the scrollbar renders as a tick, anchored to an absolute
/// scrollback row.
///
/// `abs_pos` is "lines since the very top of scrollback" (0 = oldest), the
/// stable identifier a `TrimScrollback` shifts; `status` selects the tick hue.
/// This is the single record type for a command boundary: the binary's
/// prompt-mark store re-exports it and populates it from the server's OSC 133
/// `PromptMark` stream, so the ticks are the very rows the jumps land on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandMark {
    /// Absolute scrollback row the mark is anchored to.
    pub abs_pos: usize,
    /// Resolved command outcome driving the tick colour.
    pub status: CommandStatus,
}

/// Theme-derived colours for command-status scrollbar ticks.
///
/// Derived from the active theme's ANSI palette so accessible / high-contrast
/// themes apply automatically (index 2 = green success, index 1 = red failure),
/// mirroring how the status bar derives its connected / disconnected dot
/// colours. `Unknown` is intentionally absent — it keeps the neutral tick.
#[derive(Debug, Clone, Copy)]
pub struct CommandMarkColors {
    /// Success tick colour (theme ANSI green, index 2).
    pub success: [f32; 4],
    /// Failure tick colour (theme ANSI red, index 1).
    pub failure: [f32; 4],
}

/// Fallback success colour when ANSI index 2 is unavailable.
const FALLBACK_SUCCESS: [f32; 4] = [0.4, 0.9, 0.5, 1.0];
/// Fallback failure colour when ANSI index 1 is unavailable.
const FALLBACK_FAILURE: [f32; 4] = [1.0, 0.2, 0.2, 1.0];

impl CommandMarkColors {
    /// Derive success/failure tick colours from the theme's linearised ANSI
    /// palette, falling back to fixed colours when an index is missing.
    #[must_use]
    pub fn from_ansi(ansi_colors: &[[f32; 4]; 16]) -> Self {
        Self {
            success: ansi_colors.get(2).copied().unwrap_or(FALLBACK_SUCCESS),
            failure: ansi_colors.get(1).copied().unwrap_or(FALLBACK_FAILURE),
        }
    }
}

/// Resting scrollbar thumb width in physical pixels.
///
/// Fixed rather than read from `appearance.scrollbar_width`: that key is a
/// removed key for the GPUI client (it fed the bespoke pipeline's hover-lerp
/// geometry), so it must keep deserializing harmlessly and changing nothing.
pub const SCROLLBAR_WIDTH: f32 = 6.0;

/// Minimum scrollbar thumb height in physical pixels.
const MIN_THUMB_HEIGHT: f32 = 20.0;

/// Inset from the right edge of the pane content area in physical pixels.
const RIGHT_INSET: f32 = 2.0;

/// Duration (seconds) before the scrollbar starts fading after last activity.
const FADE_DELAY_SECS: f32 = 1.5;

/// Duration (seconds) of the fade-out animation.
const FADE_DURATION_SECS: f32 = 0.3;

/// Extra width added to the scrollbar when hovering, in physical pixels.
const HOVER_EXTRA_WIDTH: f32 = 3.0;

/// Speed of the width animation (lerp factor per second).
const WIDTH_LERP_SPEED: f32 = 12.0;

/// Multiplier applied to the base width to form the drag/click hit zone.
const HIT_ZONE_MULTIPLIER: f32 = 3.0;

/// Height of a command tick in physical pixels.
const MARK_HEIGHT: f32 = 2.0;

/// Tick alpha relative to the thumb alpha (ticks read slightly dimmer).
const MARK_ALPHA_SCALE: f32 = 0.6;

/// Neutral tick colour (RGB) shared by `Unknown`-status marks.
const NEUTRAL_MARK_RGB: [f32; 3] = [0.6, 0.6, 0.8];

/// Resting-fade default alpha ceiling when a style omits its own alpha.
const DEFAULT_THUMB_ALPHA: f32 = 0.4;

const F32_CHUNK_SIZE: usize = 65_536;
const F32_CHUNK_SIZE_F32: f32 = 65_536.0;

/// Convert a scroll-unit count to `f32` without precision loss on large
/// scrollback (splitting into 16-bit chunks keeps values exact past 2^24).
fn scroll_units_f32(units: usize) -> f32 {
    let high = u16::try_from(units / F32_CHUNK_SIZE).unwrap_or(u16::MAX);
    let low = u16::try_from(units % F32_CHUNK_SIZE).unwrap_or(u16::MAX);
    f32::from(high) * F32_CHUNK_SIZE_F32 + f32::from(low)
}

/// Round an `f32` scroll target back to the nearest integer scroll-unit count,
/// clamped to `max_units`, using a binary search over the lossless conversion.
///
/// Public because a pixel scroller (the settings content pane) reuses this
/// geometry with pixels as its scroll unit, and the workspace denies the lossy
/// float-to-int casts that would otherwise do the conversion.
#[must_use]
pub fn round_scroll_units(value: f32, max_units: usize) -> usize {
    if max_units == 0 || !value.is_finite() || value <= 0.0 {
        return 0;
    }

    let max_value = scroll_units_f32(max_units);
    let target = value.min(max_value).max(0.0) + 0.5;
    let mut low = 0usize;
    let mut high = max_units;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if scroll_units_f32(mid) < target {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }
    low
}

/// Live viewport metrics a pane's terminal grid exposes to the scrollbar.
///
/// Decouples the pure geometry from the Alacritty grid: `history_size` is the
/// number of scrollback rows, `screen_lines` the visible row count, and
/// `display_offset` how many rows the viewport is scrolled up from the bottom
/// (0 = pinned at the live bottom).
#[derive(Debug, Clone, Copy)]
pub struct ScrollMetrics {
    /// Number of scrollback rows above the visible viewport.
    pub history_size: usize,
    /// Number of rows in the visible viewport.
    pub screen_lines: usize,
    /// Rows scrolled up from the live bottom (0 = at bottom).
    pub display_offset: usize,
}

/// Per-pane placement inputs for the scrollbar geometry and hit-testing.
///
/// Bundles the pane's pixel rect, live viewport [`ScrollMetrics`], and the tab
/// strip height reserved at the pane top, so geometry and hit-test entry points
/// take one placement argument plus the varying pointer/width values.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarLayout {
    /// The pane's full pixel rect.
    pub pane_rect: Rect,
    /// Live viewport metrics from the terminal grid.
    pub metrics: ScrollMetrics,
    /// Height of the tab strip reserved at the top of the pane.
    pub tab_bar_height: f32,
}

/// Per-pane scrollbar state.
#[derive(Debug)]
pub struct ScrollbarState {
    /// Current visual opacity (0.0 = invisible, 1.0 = fully visible).
    pub opacity: f32,
    /// When the idle countdown started (fade begins at `fade_start + FADE_DELAY`).
    pub fade_start: Option<Instant>,
    /// Whether the mouse is hovering over the scrollbar hit zone.
    pub hover: bool,
    /// Active thumb drag, if any.
    pub drag: Option<ScrollbarDrag>,
    /// Current animated scrollbar width (pixels). Lerps toward `target_width`.
    display_width: f32,
    /// Target scrollbar width (pixels). Set on hover enter/leave.
    target_width: f32,
    /// Last tick timestamp for width animation delta-time.
    last_tick: Option<Instant>,
}

impl Default for ScrollbarState {
    fn default() -> Self {
        Self::new()
    }
}

/// State captured when a scrollbar thumb drag begins.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarDrag {
    /// Mouse Y position when the drag started.
    pub start_mouse_y: f32,
    /// `display_offset` when the drag started.
    pub start_display_offset: usize,
}

impl ScrollbarState {
    /// Create a new scrollbar state (invisible, no drag).
    #[must_use]
    pub fn new() -> Self {
        Self {
            opacity: 0.0,
            fade_start: None,
            hover: false,
            drag: None,
            display_width: 0.0,
            target_width: 0.0,
            last_tick: None,
        }
    }

    /// Current animated width of the scrollbar thumb. Falls back to
    /// `base_width` if the animation has not been initialised yet.
    #[must_use]
    pub fn current_width(&self, base_width: f32) -> f32 {
        if self.display_width > 0.0 { self.display_width } else { base_width }
    }

    /// Signal that a scroll action occurred (keyboard, wheel, or drag).
    pub fn on_scroll_action(&mut self) {
        self.opacity = 1.0;
        self.fade_start = Some(Instant::now());
    }

    /// Signal that the mouse entered the scrollbar hit zone.
    pub fn on_hover_enter(&mut self) {
        self.hover = true;
        self.opacity = 1.0;
        self.fade_start = None;
    }

    /// Signal that the mouse left the scrollbar hit zone.
    pub fn on_hover_leave(&mut self) {
        self.hover = false;
        if self.drag.is_none() {
            self.fade_start = Some(Instant::now());
        }
    }

    /// Signal that a drag ended.
    pub fn on_drag_end(&mut self) {
        self.drag = None;
        if !self.hover {
            self.fade_start = Some(Instant::now());
        }
    }

    /// Advance the fade and width animations, using `now` as the clock so the
    /// animation is deterministic under test. Returns `true` if the scrollbar
    /// is still visible and needs further redraws.
    pub fn tick_fade_at(&mut self, display_offset: usize, now: Instant) -> bool {
        // --- Width lerp animation ---
        if self.target_width > 0.0 {
            let dt = self.last_tick.map_or(0.0, |prev| now.duration_since(prev).as_secs_f32());
            let factor = (WIDTH_LERP_SPEED * dt).min(1.0);
            self.display_width += (self.target_width - self.display_width) * factor;
        }
        self.last_tick = Some(now);

        let width_animating =
            self.target_width > 0.0 && (self.display_width - self.target_width).abs() > 0.1;

        // --- Opacity fade animation ---

        // While dragging or hovering, stay fully opaque.
        if self.drag.is_some() || self.hover {
            self.opacity = 1.0;
            return true;
        }

        // At bottom with no hover/drag — snap to invisible.
        if display_offset == 0
            && self.fade_start.is_none()
            && self.opacity <= 0.0
            && !width_animating
        {
            return false;
        }

        let Some(start) = self.fade_start else {
            // No fade timer, but opacity > 0 (e.g. just scrolled).
            return self.opacity > 0.0 || width_animating;
        };

        let elapsed = now.saturating_duration_since(start).as_secs_f32();
        if elapsed < FADE_DELAY_SECS {
            // Still in the idle delay period.
            return true;
        }

        let fade_progress = (elapsed - FADE_DELAY_SECS) / FADE_DURATION_SECS;
        if fade_progress >= 1.0 {
            self.opacity = 0.0;
            self.fade_start = None;
            return width_animating;
        }

        self.opacity = 1.0 - fade_progress;
        true
    }

    /// Advance the fade and width animations against the real clock.
    pub fn tick_fade(&mut self, display_offset: usize) -> bool {
        self.tick_fade_at(display_offset, Instant::now())
    }
}

/// Computed geometry for a scrollbar thumb.
#[derive(Debug, Clone, Copy)]
pub struct ThumbGeometry {
    /// X position of the thumb (right-aligned within the pane).
    pub x: f32,
    /// Y position of the thumb top edge.
    pub y: f32,
    /// Width of the thumb in pixels.
    pub width: f32,
    /// Height of the thumb in pixels.
    pub height: f32,
    /// Top of the track (content area top).
    pub track_top: f32,
    /// Height of the track.
    pub track_height: f32,
}

/// Compute thumb geometry for a pane, or `None` if it has no scrollback.
///
/// `pane_rect` is the pane's full pixel rect and `tab_bar_height` reserves the
/// tab strip at its top; `scrollbar_width` is the (possibly hover-animated)
/// thumb width.
#[must_use]
pub fn compute_thumb(layout: &ScrollbarLayout, scrollbar_width: f32) -> Option<ThumbGeometry> {
    let ScrollbarLayout { pane_rect, metrics, tab_bar_height } = *layout;
    if metrics.history_size == 0 {
        return None;
    }

    let track_top = pane_rect.y + tab_bar_height;
    let track_height = (pane_rect.height - tab_bar_height).max(1.0);

    let total = scroll_units_f32(metrics.history_size.saturating_add(metrics.screen_lines));
    let thumb_height =
        (scroll_units_f32(metrics.screen_lines) / total * track_height).max(MIN_THUMB_HEIGHT);
    let available = (track_height - thumb_height).max(0.0);

    let ratio =
        1.0 - (scroll_units_f32(metrics.display_offset) / scroll_units_f32(metrics.history_size));
    let thumb_y = (track_top + ratio * available).clamp(track_top, track_top + available);

    let thumb_x = pane_rect.x + pane_rect.width - scrollbar_width - RIGHT_INSET;

    Some(ThumbGeometry {
        x: thumb_x,
        y: thumb_y,
        width: scrollbar_width,
        height: thumb_height,
        track_top,
        track_height,
    })
}

/// Theme-derived styling for one pane's scrollbar render pass.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarStyle {
    /// Base scrollbar thumb width in physical pixels.
    pub width: f32,
    /// Base scrollbar thumb colour (alpha is the resting fade ceiling).
    pub color: [f32; 4],
    /// Theme-derived success/failure colours for command-status ticks.
    pub command_mark_colors: CommandMarkColors,
}

impl ScrollbarStyle {
    /// Resolve the whole scrollbar palette from the active theme.
    ///
    /// The thumb takes the theme's derived `chrome.scrollbar` slot (a
    /// 40 %-alpha foreground tone, which doubles as the resting fade ceiling)
    /// and the ticks take the ANSI green/red, so a theme switch — including an
    /// accessible high-contrast one — re-colours the whole overlay with no
    /// scrollbar-specific configuration.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            width: SCROLLBAR_WIDTH,
            color: theme.chrome.scrollbar,
            command_mark_colors: CommandMarkColors::from_ansi(&theme.ansi_colors),
        }
    }
}

/// A single rounded quad the paint path fills for the scrollbar.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarQuad {
    /// Pixel rect of the quad.
    pub rect: Rect,
    /// Linear RGBA fill (alpha already folded through the fade opacity).
    pub color: [f32; 4],
    /// Corner radius in pixels.
    pub corner_radius: f32,
}

/// Render-ready scrollbar geometry: the thumb quad plus command ticks.
///
/// Produced by [`build_scrollbar_render`] and lowered onto GPUI quads by the
/// paint path; empty when the scrollbar is invisible or has no scrollback.
#[derive(Debug, Clone)]
pub struct ScrollbarRender {
    /// The scrollbar thumb quad.
    pub thumb: ScrollbarQuad,
    /// Command-boundary tick quads, in input order.
    pub ticks: Vec<ScrollbarQuad>,
}

/// Build the render geometry for a single pane's scrollbar.
///
/// Updates the state's width-animation target from the current hover flag, then
/// returns the thumb quad and command ticks with the fade opacity folded into
/// their alpha. Returns `None` when the scrollbar is invisible or the pane has
/// no scrollback.
pub fn build_scrollbar_render(
    layout: &ScrollbarLayout,
    marks: &[CommandMark],
    state: &mut ScrollbarState,
    style: &ScrollbarStyle,
) -> Option<ScrollbarRender> {
    // Update width animation targets based on hover state.
    let hover_width = style.width + HOVER_EXTRA_WIDTH;
    state.target_width = if state.hover { hover_width } else { style.width };
    if state.display_width <= 0.0 {
        state.display_width = style.width;
    }

    if state.opacity <= 0.0 {
        return None;
    }

    let animated_width = state.current_width(style.width);
    let thumb = compute_thumb(layout, animated_width)?;

    // Apply fade opacity to the base scrollbar colour alpha.
    let alpha = style.color.get(3).copied().unwrap_or(DEFAULT_THUMB_ALPHA) * state.opacity;
    let color = [
        style.color.first().copied().unwrap_or(0.0),
        style.color.get(1).copied().unwrap_or(0.0),
        style.color.get(2).copied().unwrap_or(0.0),
        alpha,
    ];

    let thumb_quad = ScrollbarQuad {
        rect: Rect { x: thumb.x, y: thumb.y, width: thumb.width, height: thumb.height },
        color,
        corner_radius: animated_width / 2.0,
    };

    let ticks = build_command_ticks(&thumb, layout.metrics, marks, style, alpha);
    Some(ScrollbarRender { thumb: thumb_quad, ticks })
}

/// Build the command-boundary tick quads for a thumb geometry.
///
/// Each mark's `abs_pos` maps onto the full track, clamped so a stale position
/// (from before a resize shrank scrollback) cannot render outside the track.
fn build_command_ticks(
    thumb: &ThumbGeometry,
    metrics: ScrollMetrics,
    marks: &[CommandMark],
    style: &ScrollbarStyle,
    alpha: f32,
) -> Vec<ScrollbarQuad> {
    if marks.is_empty() {
        return Vec::new();
    }

    let total = scroll_units_f32(metrics.history_size.saturating_add(metrics.screen_lines));
    let tick_alpha = alpha * MARK_ALPHA_SCALE;
    let neutral_color = [NEUTRAL_MARK_RGB[0], NEUTRAL_MARK_RGB[1], NEUTRAL_MARK_RGB[2], tick_alpha];
    let status_rgba = |base: [f32; 4]| {
        [
            base.first().copied().unwrap_or(0.0),
            base.get(1).copied().unwrap_or(0.0),
            base.get(2).copied().unwrap_or(0.0),
            tick_alpha,
        ]
    };

    marks
        .iter()
        .map(|mark| {
            let mark_color = match mark.status {
                CommandStatus::Unknown => neutral_color,
                CommandStatus::Success => status_rgba(style.command_mark_colors.success),
                CommandStatus::Failure => status_rgba(style.command_mark_colors.failure),
            };
            let ratio = scroll_units_f32(mark.abs_pos) / total;
            let mark_y = (thumb.track_top + ratio * thumb.track_height)
                .clamp(thumb.track_top, thumb.track_top + thumb.track_height - MARK_HEIGHT);
            ScrollbarQuad {
                rect: Rect { x: thumb.x, y: mark_y, width: thumb.width, height: MARK_HEIGHT },
                color: mark_color,
                corner_radius: 1.0,
            }
        })
        .collect()
}

/// Hit-test whether a point is within the scrollbar drag/click hit zone.
///
/// The hit zone is `scrollbar_width * 3` wide, anchored to the right edge (3x
/// hit-zone padding). Returns `true` only when the point is in the zone and the
/// pane has scrollback.
#[must_use]
pub fn hit_test_scrollbar(layout: &ScrollbarLayout, x: f32, y: f32, scrollbar_width: f32) -> bool {
    let ScrollbarLayout { pane_rect, metrics, tab_bar_height } = *layout;
    if metrics.history_size == 0 {
        return false;
    }

    let track_top = pane_rect.y + tab_bar_height;
    let track_bottom = pane_rect.y + pane_rect.height;
    let hit_zone_width = scrollbar_width * HIT_ZONE_MULTIPLIER;
    let hit_zone_left = pane_rect.x + pane_rect.width - hit_zone_width - RIGHT_INSET;

    x >= hit_zone_left && x <= pane_rect.x + pane_rect.width && y >= track_top && y <= track_bottom
}

/// Hit-test whether a point is on the scrollbar thumb itself.
#[must_use]
pub fn hit_test_thumb(layout: &ScrollbarLayout, x: f32, y: f32, scrollbar_width: f32) -> bool {
    let Some(thumb) = compute_thumb(layout, scrollbar_width) else {
        return false;
    };

    x >= thumb.x && x <= thumb.x + thumb.width && y >= thumb.y && y <= thumb.y + thumb.height
}

/// Compute a target `display_offset` from a click Y position on the track.
///
/// Returns the offset that positions the thumb so the click point maps onto the
/// track (top = oldest scrollback, bottom = live view).
#[must_use]
pub fn offset_from_track_click(
    layout: &ScrollbarLayout,
    click_y: f32,
    scrollbar_width: f32,
) -> usize {
    let Some(thumb) = compute_thumb(layout, scrollbar_width) else {
        return 0;
    };

    let history_size = layout.metrics.history_size;
    if history_size == 0 || thumb.track_height <= thumb.height {
        return 0;
    }

    let available = thumb.track_height - thumb.height;
    // Ratio: 0.0 = bottom (display_offset=0), 1.0 = top (display_offset=history_size).
    let ratio = 1.0 - ((click_y - thumb.track_top) / available).clamp(0.0, 1.0);
    let offset = round_scroll_units(ratio * scroll_units_f32(history_size), history_size);
    offset.min(history_size)
}

/// Compute a target `display_offset` from a drag delta.
///
/// `drag` is the captured state from drag start; `current_mouse_y` is the
/// current Y position. Dragging down decreases the offset (scroll toward the
/// live bottom).
#[must_use]
pub fn offset_from_drag(
    layout: &ScrollbarLayout,
    drag: &ScrollbarDrag,
    current_mouse_y: f32,
    scrollbar_width: f32,
) -> usize {
    let Some(thumb) = compute_thumb(layout, scrollbar_width) else {
        return drag.start_display_offset;
    };

    let history_size = layout.metrics.history_size;
    if history_size == 0 || thumb.track_height <= thumb.height {
        return drag.start_display_offset;
    }

    let available = thumb.track_height - thumb.height;
    let delta_y = current_mouse_y - drag.start_mouse_y;
    // Dragging down (positive delta_y) decreases display_offset.
    let delta_lines = -(delta_y * scroll_units_f32(history_size) / available);

    let new_offset = scroll_units_f32(drag.start_display_offset) + delta_lines;
    round_scroll_units(new_offset.max(0.0), history_size).min(history_size)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::assert_rgba_eq;

    const WIDTH: f32 = 6.0;
    const TAB_BAR: f32 = 30.0;

    fn metrics(history: usize, screen: usize, offset: usize) -> ScrollMetrics {
        ScrollMetrics { history_size: history, screen_lines: screen, display_offset: offset }
    }

    fn layout(history: usize, screen: usize, offset: usize) -> ScrollbarLayout {
        ScrollbarLayout {
            pane_rect: Rect { x: 100.0, y: 50.0, width: 400.0, height: 600.0 },
            metrics: metrics(history, screen, offset),
            tab_bar_height: TAB_BAR,
        }
    }

    fn style() -> ScrollbarStyle {
        ScrollbarStyle {
            width: WIDTH,
            color: [0.5, 0.5, 0.5, 0.4],
            command_mark_colors: CommandMarkColors {
                success: [0.0, 1.0, 0.0, 1.0],
                failure: [1.0, 0.0, 0.0, 1.0],
            },
        }
    }

    // @lat: [[test#GPUI Command Scrollbar#No scrollback yields no thumb]]
    #[gpui::test]
    fn no_scrollback_yields_no_thumb() {
        // History of zero rows: nothing to scroll, so no thumb geometry.
        let layout = layout(0, 24, 0);
        assert!(compute_thumb(&layout, WIDTH).is_none());
        // Hit-testing the right edge still misses because there is no scrollback.
        assert!(!hit_test_scrollbar(&layout, 498.0, 300.0, WIDTH));
        // Even a wheel/page action that pulses the fade cannot paint a bar.
        let mut state = ScrollbarState::new();
        state.on_scroll_action();
        assert!(build_scrollbar_render(&layout, &[], &mut state, &style()).is_none());
    }

    // @lat: [[test#GPUI Command Scrollbar#Thumb sizes and positions from the viewport]]
    #[gpui::test]
    fn thumb_sizes_and_positions_from_the_viewport() {
        // 100 scrollback rows + 24 visible over a 570px track (600 - 30 tab bar).
        let track_top = 50.0 + TAB_BAR;
        let track_height: f32 = 600.0 - TAB_BAR;

        // At the live bottom (offset 0) the thumb sits at the track bottom.
        let bottom = compute_thumb(&layout(100, 24, 0), WIDTH).unwrap();
        let expected_height = (24.0 / 124.0 * track_height).max(MIN_THUMB_HEIGHT);
        assert!((bottom.height - expected_height).abs() < 0.01);
        let available = track_height - expected_height;
        assert!((bottom.y - (track_top + available)).abs() < 0.01);
        // Right-aligned inside the pane with the 2px inset.
        assert!((bottom.x - (100.0 + 400.0 - WIDTH - RIGHT_INSET)).abs() < f32::EPSILON);

        // Fully scrolled up (offset == history) pins the thumb to the track top.
        let top = compute_thumb(&layout(100, 24, 100), WIDTH).unwrap();
        assert!((top.y - track_top).abs() < 0.01);
    }

    // @lat: [[test#GPUI Command Scrollbar#Track click maps to a scroll offset]]
    #[gpui::test]
    fn track_click_maps_to_a_scroll_offset() {
        let l = layout(100, 24, 0);
        let thumb = compute_thumb(&l, WIDTH).unwrap();

        // Clicking at (or above) the track top jumps to the oldest scrollback.
        assert_eq!(offset_from_track_click(&l, thumb.track_top, WIDTH), 100);
        // Clicking at the track bottom returns to the live view.
        let bottom_y = thumb.track_top + thumb.track_height;
        assert_eq!(offset_from_track_click(&l, bottom_y, WIDTH), 0);
        // A mid-track click lands part-way up the history.
        let mid = offset_from_track_click(&l, thumb.track_top + thumb.track_height / 2.0, WIDTH);
        assert!(mid > 0 && mid < 100);
    }

    // @lat: [[test#GPUI Command Scrollbar#Drag maps vertical delta to offset]]
    #[gpui::test]
    fn drag_maps_vertical_delta_to_offset() {
        let l = layout(100, 24, 50);
        let thumb = compute_thumb(&l, WIDTH).unwrap();
        let available = thumb.track_height - thumb.height;
        let drag = ScrollbarDrag { start_mouse_y: 300.0, start_display_offset: 50 };

        // No movement keeps the starting offset.
        assert_eq!(offset_from_drag(&l, &drag, 300.0, WIDTH), 50);
        // Dragging down by one track-worth of pixels scrolls fully to the bottom.
        assert_eq!(offset_from_drag(&l, &drag, 300.0 + available, WIDTH), 0);
        // Dragging up increases the offset toward the top of history.
        let up = offset_from_drag(&l, &drag, 300.0 - available / 2.0, WIDTH);
        assert!(up > 50 && up <= 100);
    }

    // @lat: [[test#GPUI Command Scrollbar#Hit zone widens the right edge threefold]]
    #[gpui::test]
    fn hit_zone_widens_the_right_edge_threefold() {
        let l = layout(100, 24, 10);
        let right = 100.0 + 400.0;
        // Within the 3x-width zone (18px) inset 2px from the right edge.
        assert!(hit_test_scrollbar(&l, right - 5.0, 300.0, WIDTH));
        assert!(hit_test_scrollbar(&l, right - 19.0, 300.0, WIDTH));
        // Just left of the 3x zone (20px in) misses.
        assert!(!hit_test_scrollbar(&l, right - 21.0, 300.0, WIDTH));
        // Above the track top (inside the tab bar) misses.
        assert!(!hit_test_scrollbar(&l, right - 5.0, 60.0, WIDTH));
    }

    // @lat: [[test#GPUI Command Scrollbar#Thumb hit test tracks the thumb rect]]
    #[gpui::test]
    fn thumb_hit_test_tracks_the_thumb_rect() {
        let l = layout(100, 24, 0);
        let thumb = compute_thumb(&l, WIDTH).unwrap();
        // A point inside the thumb rect hits.
        assert!(hit_test_thumb(&l, thumb.x + 1.0, thumb.y + 1.0, WIDTH));
        // A point above the thumb (still in the track) misses the thumb itself.
        assert!(!hit_test_thumb(&l, thumb.x + 1.0, thumb.track_top, WIDTH));
    }

    // @lat: [[test#GPUI Command Scrollbar#Command ticks colour by status and shift with trim]]
    #[gpui::test]
    fn command_ticks_colour_by_status_and_shift_with_trim() {
        let l = layout(100, 24, 100);
        let mut state = ScrollbarState::new();
        state.opacity = 1.0;
        let st = style();
        let marks = vec![
            CommandMark { abs_pos: 10, status: CommandStatus::Success },
            CommandMark { abs_pos: 50, status: CommandStatus::Failure },
            CommandMark { abs_pos: 90, status: CommandStatus::Unknown },
        ];

        let render = build_scrollbar_render(&l, &marks, &mut state, &st).unwrap();
        assert_eq!(render.ticks.len(), 3);
        // Success tick pulls the theme green (RGB), Failure the theme red, and
        // Unknown the neutral RGB — the tick alpha is uniform across statuses.
        let tick_alpha = st.color[3] * MARK_ALPHA_SCALE;
        assert_rgba_eq(render.ticks[0].color, [0.0, 1.0, 0.0, tick_alpha]);
        assert_rgba_eq(render.ticks[1].color, [1.0, 0.0, 0.0, tick_alpha]);
        assert_rgba_eq(
            render.ticks[2].color,
            [NEUTRAL_MARK_RGB[0], NEUTRAL_MARK_RGB[1], NEUTRAL_MARK_RGB[2], tick_alpha],
        );

        // The tick Y is monotonic in abs_pos: a higher row sits lower on the track.
        assert!(render.ticks[0].rect.y < render.ticks[1].rect.y);
        assert!(render.ticks[1].rect.y < render.ticks[2].rect.y);

        // Trimming 40 oldest rows shifts every surviving mark's abs_pos down by
        // 40 (mirroring session_lifecycle), so the ticks move up the track.
        let trimmed = vec![
            CommandMark { abs_pos: 10, status: CommandStatus::Failure },
            CommandMark { abs_pos: 50, status: CommandStatus::Unknown },
        ];
        let after = build_scrollbar_render(&layout(60, 24, 60), &trimmed, &mut state, &st).unwrap();
        // Both surviving marks stay ordered over the new, smaller total.
        assert_eq!(after.ticks.len(), 2);
        assert!(after.ticks[0].rect.y < after.ticks[1].rect.y);
    }

    // @lat: [[test#GPUI Command Scrollbar#Stale mark position clamps inside the track]]
    #[gpui::test]
    fn stale_mark_position_clamps_inside_the_track() {
        let l = layout(100, 24, 100);
        let mut state = ScrollbarState::new();
        state.opacity = 1.0;
        let st = style();
        // abs_pos far past history (stale, pre-resize) must not render past the track.
        let marks = vec![CommandMark { abs_pos: 100_000, status: CommandStatus::Success }];
        let render = build_scrollbar_render(&l, &marks, &mut state, &st).unwrap();
        let tick = render.ticks[0];
        let thumb = compute_thumb(&l, state.current_width(st.width)).unwrap();
        assert!(tick.rect.y <= thumb.track_top + thumb.track_height - MARK_HEIGHT + 0.01);
        assert!(tick.rect.y >= thumb.track_top - 0.01);
    }

    // @lat: [[test#GPUI Command Scrollbar#Invisible scrollbar renders nothing]]
    #[gpui::test]
    fn invisible_scrollbar_renders_nothing() {
        let mut state = ScrollbarState::new();
        // Opacity starts at zero, so no render even with scrollback and marks.
        let marks = vec![CommandMark { abs_pos: 5, status: CommandStatus::Success }];
        assert!(
            build_scrollbar_render(&layout(100, 24, 10), &marks, &mut state, &style()).is_none()
        );
    }

    // @lat: [[test#GPUI Command Scrollbar#Fade idles then fades over the configured windows]]
    #[gpui::test]
    fn fade_idles_then_fades_over_the_configured_windows() {
        let mut state = ScrollbarState::new();
        let t0 = Instant::now();
        state.on_scroll_action();
        // A scroll action snaps to full opacity and arms the idle timer.
        assert!((state.opacity - 1.0).abs() < f32::EPSILON);

        // During the 1.5s idle delay the scrollbar stays fully visible.
        assert!(state.tick_fade_at(10, t0 + Duration::from_secs(1)));
        assert!((state.opacity - 1.0).abs() < f32::EPSILON);

        // Part-way through the 0.3s fade the opacity is between 0 and 1.
        assert!(state.tick_fade_at(10, t0 + Duration::from_millis(1_650)));
        assert!(state.opacity > 0.0 && state.opacity < 1.0);

        // Past the full fade window the scrollbar is invisible and settles.
        state.tick_fade_at(10, t0 + Duration::from_secs(2));
        assert!(state.opacity <= 0.0);
    }

    // @lat: [[test#GPUI Command Scrollbar#Hover holds opacity and widens the thumb]]
    #[gpui::test]
    fn hover_holds_opacity_and_widens_the_thumb() {
        let mut state = ScrollbarState::new();
        let st = style();
        let t0 = Instant::now();
        state.on_hover_enter();
        // Hover pins full opacity and clears the fade timer.
        assert!((state.opacity - 1.0).abs() < f32::EPSILON);
        assert!(state.fade_start.is_none());

        // The width target jumps to base + hover extra; the display width lerps
        // toward it across ticks and stays visible while hovering.
        build_scrollbar_render(&layout(100, 24, 10), &[], &mut state, &st);
        assert!((state.target_width - (st.width + HOVER_EXTRA_WIDTH)).abs() < f32::EPSILON);
        let w0 = state.current_width(st.width);
        assert!(state.tick_fade_at(10, t0 + Duration::from_millis(50)));
        let w1 = state.current_width(st.width);
        assert!(w1 >= w0 && w1 <= st.width + HOVER_EXTRA_WIDTH);

        // Leaving hover re-arms the fade timer and relaxes the width target back.
        state.on_hover_leave();
        assert!(state.fade_start.is_some());
        build_scrollbar_render(&layout(100, 24, 10), &[], &mut state, &st);
        assert!((state.target_width - st.width).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Command Scrollbar#Mark colours fall back without an ANSI palette]]
    #[gpui::test]
    fn mark_colours_fall_back_without_an_ansi_palette() {
        // A palette carrying explicit green/red at indices 2/1 is used directly.
        let mut ansi = [[0.0_f32, 0.0, 0.0, 1.0]; 16];
        ansi[1] = [0.9, 0.1, 0.1, 1.0];
        ansi[2] = [0.1, 0.8, 0.2, 1.0];
        let colors = CommandMarkColors::from_ansi(&ansi);
        assert_rgba_eq(colors.success, [0.1, 0.8, 0.2, 1.0]);
        assert_rgba_eq(colors.failure, [0.9, 0.1, 0.1, 1.0]);
    }
}
