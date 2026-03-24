//! macOS-style overlay scrollbar: state, geometry, rendering, and hit-testing.
//!
//! The scrollbar is a non-reserving overlay on the right edge of each pane's
//! content area. It fades in on scroll, fades out after inactivity, and
//! supports click-to-jump and drag-to-scroll.

use std::time::Instant;

use alacritty_terminal::grid::Dimensions as _;
use scribe_renderer::types::CellInstance;

use crate::pane::{Pane, TAB_BAR_HEIGHT};

/// Minimum scrollbar thumb height in physical pixels.
#[allow(dead_code, reason = "used by compute_thumb, wired in later tasks")]
const MIN_THUMB_HEIGHT: f32 = 20.0;

/// Inset from the right edge of the pane content area in physical pixels.
#[allow(dead_code, reason = "used by compute_thumb and hit_test_scrollbar, wired in later tasks")]
const RIGHT_INSET: f32 = 2.0;

/// Duration (seconds) before the scrollbar starts fading after last activity.
#[allow(dead_code, reason = "used by tick_fade, wired in later tasks")]
const FADE_DELAY_SECS: f32 = 1.5;

/// Duration (seconds) of the fade-out animation.
#[allow(dead_code, reason = "used by tick_fade, wired in later tasks")]
const FADE_DURATION_SECS: f32 = 0.3;

/// Per-pane scrollbar state.
pub struct ScrollbarState {
    /// Current visual opacity (0.0 = invisible, 1.0 = fully visible).
    pub opacity: f32,
    /// When the idle countdown started (fade begins at `fade_start + FADE_DELAY`).
    #[allow(dead_code, reason = "read by tick_fade, wired in later tasks")]
    pub fade_start: Option<Instant>,
    /// Whether the mouse is hovering over the scrollbar hit zone.
    #[allow(dead_code, reason = "read by tick_fade and on_hover_leave, wired in later tasks")]
    pub hover: bool,
    /// Active thumb drag, if any.
    #[allow(dead_code, reason = "read by tick_fade and on_drag_end, wired in later tasks")]
    pub drag: Option<ScrollbarDrag>,
}

/// State captured when a scrollbar thumb drag begins.
pub struct ScrollbarDrag {
    /// Mouse Y position when the drag started.
    #[allow(dead_code, reason = "read by offset_from_drag, wired in later tasks")]
    pub start_mouse_y: f32,
    /// `display_offset` when the drag started.
    #[allow(dead_code, reason = "read by offset_from_drag, wired in later tasks")]
    pub start_display_offset: usize,
}

impl ScrollbarState {
    /// Create a new scrollbar state (invisible, no drag).
    pub fn new() -> Self {
        Self { opacity: 0.0, fade_start: None, hover: false, drag: None }
    }

    /// Signal that a scroll action occurred (keyboard, wheel, or drag).
    #[allow(dead_code, reason = "called from scroll event handler, wired in later tasks")]
    pub fn on_scroll_action(&mut self) {
        self.opacity = 1.0;
        self.fade_start = Some(Instant::now());
    }

    /// Signal that the mouse entered the scrollbar hit zone.
    #[allow(dead_code, reason = "called from mouse move handler, wired in later tasks")]
    pub fn on_hover_enter(&mut self) {
        self.hover = true;
        self.opacity = 1.0;
        self.fade_start = None;
    }

    /// Signal that the mouse left the scrollbar hit zone.
    #[allow(dead_code, reason = "called from mouse move handler, wired in later tasks")]
    pub fn on_hover_leave(&mut self) {
        self.hover = false;
        if self.drag.is_none() {
            self.fade_start = Some(Instant::now());
        }
    }

    /// Signal that a drag ended.
    #[allow(dead_code, reason = "called from mouse button release handler, wired in later tasks")]
    pub fn on_drag_end(&mut self) {
        self.drag = None;
        if !self.hover {
            self.fade_start = Some(Instant::now());
        }
    }

    /// Advance the fade animation. Returns `true` if the scrollbar is still
    /// visible and needs further redraws.
    #[allow(dead_code, reason = "called from frame animation loop, wired in later tasks")]
    pub fn tick_fade(&mut self, display_offset: usize) -> bool {
        // While dragging or hovering, stay fully opaque.
        if self.drag.is_some() || self.hover {
            self.opacity = 1.0;
            return true;
        }

        // At bottom with no hover/drag — snap to invisible.
        if display_offset == 0 && self.fade_start.is_none() && self.opacity <= 0.0 {
            return false;
        }

        let Some(start) = self.fade_start else {
            // No fade timer, but opacity > 0 (e.g. just scrolled).
            return self.opacity > 0.0;
        };

        let elapsed = start.elapsed().as_secs_f32();
        if elapsed < FADE_DELAY_SECS {
            // Still in the idle delay period.
            return true;
        }

        let fade_progress = (elapsed - FADE_DELAY_SECS) / FADE_DURATION_SECS;
        if fade_progress >= 1.0 {
            self.opacity = 0.0;
            self.fade_start = None;
            return false;
        }

        self.opacity = 1.0 - fade_progress;
        true
    }
}

/// Computed geometry for a scrollbar thumb.
struct ThumbGeometry {
    /// X position of the thumb (right-aligned within pane).
    x: f32,
    /// Y position of the thumb top edge.
    y: f32,
    /// Width of the thumb in pixels.
    width: f32,
    /// Height of the thumb in pixels.
    height: f32,
    /// Top of the track (content area top).
    track_top: f32,
    /// Height of the track.
    track_height: f32,
}

/// Compute thumb geometry for a pane, or `None` if the pane has no scrollback.
#[allow(
    clippy::cast_precision_loss,
    reason = "grid dimensions and display_offset are small positive integers fitting in f32"
)]
fn compute_thumb(pane: &Pane, scrollbar_width: f32) -> Option<ThumbGeometry> {
    let history_size = pane.term.grid().history_size();
    if history_size == 0 {
        return None;
    }

    let screen_lines = pane.term.grid().screen_lines();
    let display_offset = pane.term.grid().display_offset();

    let track_top = pane.rect.y + TAB_BAR_HEIGHT;
    let track_height = (pane.rect.height - TAB_BAR_HEIGHT).max(1.0);

    let total = (history_size + screen_lines) as f32;
    let thumb_height = (screen_lines as f32 / total * track_height).max(MIN_THUMB_HEIGHT);
    let available = track_height - thumb_height;

    let ratio = 1.0 - (display_offset as f32 / history_size as f32);
    let thumb_y = (track_top + ratio * available).clamp(track_top, track_top + available);

    let thumb_x = pane.rect.x + pane.rect.width - scrollbar_width - RIGHT_INSET;

    Some(ThumbGeometry {
        x: thumb_x,
        y: thumb_y,
        width: scrollbar_width,
        height: thumb_height,
        track_top,
        track_height,
    })
}

/// Build scrollbar instances for a single pane and push them into `out`.
///
/// Does nothing if the pane has no scrollback or the scrollbar is invisible.
#[allow(dead_code, reason = "called from frame builder, wired in later tasks")]
pub fn build_scrollbar_instances(
    out: &mut Vec<CellInstance>,
    pane: &Pane,
    scrollbar_width: f32,
    scrollbar_color: [f32; 4],
) {
    if pane.scrollbar_state.opacity <= 0.0 {
        return;
    }

    let Some(thumb) = compute_thumb(pane, scrollbar_width) else {
        return;
    };

    // Apply fade opacity to the base scrollbar color alpha.
    let alpha = scrollbar_color.get(3).copied().unwrap_or(0.4) * pane.scrollbar_state.opacity;
    let color = [
        scrollbar_color.first().copied().unwrap_or(0.0),
        scrollbar_color.get(1).copied().unwrap_or(0.0),
        scrollbar_color.get(2).copied().unwrap_or(0.0),
        alpha,
    ];

    out.push(CellInstance {
        pos: [thumb.x, thumb.y],
        size: [thumb.width, thumb.height],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        fg_color: color,
        bg_color: color,
    });
}

/// Hit-test whether a point is within the scrollbar hit zone of a pane.
///
/// The hit zone is `scrollbar_width * 3` wide, anchored to the right edge.
/// Returns `true` if the point is in the zone AND the pane has scrollback.
#[allow(dead_code, reason = "called from mouse move handler, wired in later tasks")]
pub fn hit_test_scrollbar(pane: &Pane, x: f32, y: f32, scrollbar_width: f32) -> bool {
    let history_size = pane.term.grid().history_size();
    if history_size == 0 {
        return false;
    }

    let track_top = pane.rect.y + TAB_BAR_HEIGHT;
    let track_bottom = pane.rect.y + pane.rect.height;
    let hit_zone_width = scrollbar_width * 3.0;
    let hit_zone_left = pane.rect.x + pane.rect.width - hit_zone_width - RIGHT_INSET;

    x >= hit_zone_left && x <= pane.rect.x + pane.rect.width && y >= track_top && y <= track_bottom
}

/// Hit-test whether a point is on the scrollbar thumb itself.
///
/// Returns `true` if the point is within the thumb rectangle.
#[allow(dead_code, reason = "called from mouse button press handler, wired in later tasks")]
pub fn hit_test_thumb(pane: &Pane, x: f32, y: f32, scrollbar_width: f32) -> bool {
    let Some(thumb) = compute_thumb(pane, scrollbar_width) else {
        return false;
    };

    x >= thumb.x && x <= thumb.x + thumb.width && y >= thumb.y && y <= thumb.y + thumb.height
}

/// Compute a target `display_offset` from a click Y position on the track.
///
/// Returns the offset that would position the thumb center at the click point.
#[allow(
    dead_code,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "grid dimensions are small positive integers; result is clamped to valid range; \
              called from mouse click handler wired in later tasks"
)]
pub fn offset_from_track_click(pane: &Pane, click_y: f32, scrollbar_width: f32) -> usize {
    let Some(thumb) = compute_thumb(pane, scrollbar_width) else {
        return 0;
    };

    let history_size = pane.term.grid().history_size();
    if history_size == 0 || thumb.track_height <= thumb.height {
        return 0;
    }

    let available = thumb.track_height - thumb.height;
    // Ratio: 0.0 = bottom (display_offset=0), 1.0 = top (display_offset=history_size)
    let ratio = 1.0 - ((click_y - thumb.track_top) / available).clamp(0.0, 1.0);
    let offset = (ratio * history_size as f32).round() as usize;
    offset.min(history_size)
}

/// Compute a target `display_offset` from a drag delta.
///
/// `drag` is the captured state from drag start. `current_mouse_y` is the
/// current Y position.
#[allow(
    dead_code,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "grid dimensions are small positive integers; result is clamped to valid range; \
              called from mouse drag handler wired in later tasks"
)]
pub fn offset_from_drag(
    pane: &Pane,
    drag: &ScrollbarDrag,
    current_mouse_y: f32,
    scrollbar_width: f32,
) -> usize {
    let Some(thumb) = compute_thumb(pane, scrollbar_width) else {
        return drag.start_display_offset;
    };

    let history_size = pane.term.grid().history_size();
    if history_size == 0 || thumb.track_height <= thumb.height {
        return drag.start_display_offset;
    }

    let available = thumb.track_height - thumb.height;
    let delta_y = current_mouse_y - drag.start_mouse_y;
    // Dragging down (positive delta_y) decreases display_offset (scroll toward bottom).
    let delta_lines = -(delta_y * history_size as f32 / available);

    let new_offset = drag.start_display_offset as f32 + delta_lines;
    (new_offset.round().max(0.0) as usize).min(history_size)
}
