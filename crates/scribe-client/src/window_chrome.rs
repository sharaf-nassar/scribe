//! Vertical geometry of the terminal window: the chrome bands and the default
//! window size that leaves room for both them and the whole terminal grid.
//!
//! The shell stacks the custom [titlebar](crate::titlebar), terminal grid, and
//! window [status bar](crate::status_bar) in a flex column, plus the optional
//! [prompt bar](crate::prompt_bar) between the grid and status bar. The grid is
//! the only flex-grown band, so every pixel the chrome takes is a pixel the grid
//! does not get; sizing the window from the grid alone therefore pushed the
//! bottom rows (and, on a smaller window, the bands themselves) off screen.
//!
//! This module is the single place those band heights are stated, so the
//! render path and the startup window size cannot drift apart.

use crate::titlebar::TITLEBAR_HEIGHT;

/// Height of the window status bar band, including its 1px top hairline —
/// GPUI lays divs out border-box, so the border is inside this number.
pub const STATUS_BAR_HEIGHT: f32 = 24.0;

/// Smallest window edge the startup size is allowed to collapse to, so a
/// nonsense font metric or a tiny virtual display still yields a usable window.
const MIN_WINDOW_EDGE: f32 = 240.0;

/// Sub-pixel slack absorbed before rounding a grid extent up.
///
/// `120 * (14.0 * 0.6)` lands 4e-5 *above* 1008.0 in `f32`, and a bare `ceil()`
/// would answer that float noise with a whole extra column of window. Nothing
/// in the grid is placed to a hundredth of a pixel, so anything under this is
/// rounding error rather than a row or column that needs room.
const PIXEL_EPSILON: f32 = 0.01;

/// Round a grid extent up to a whole pixel, ignoring [`PIXEL_EPSILON`] of
/// float noise.
fn ceil_pixels(value: f32) -> f32 {
    (value - PIXEL_EPSILON).ceil().max(0.0)
}

/// A window's inner size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSize {
    /// Inner width.
    pub width: f32,
    /// Inner height.
    pub height: f32,
}

/// Total height of the chrome bands that are always present.
///
/// The prompt bar is deliberately excluded: it exists only while the attached
/// pane has prompts, so reserving its rows up front would leave a permanent
/// dead band under the grid. When it does appear it takes its rows from the
/// flex-grown grid, and the bands below it stay on screen because each one is
/// laid out `flex_none`.
#[must_use]
pub fn chrome_height() -> f32 {
    TITLEBAR_HEIGHT + STATUS_BAR_HEIGHT
}

/// The startup window's inner size: the whole `cols`x`rows` grid at these cell
/// metrics, plus every always-present chrome band.
///
/// Cell metrics come from the live `GridFont` the grid actually paints with,
/// not from the integer cell size reported to the server, because the painted
/// metrics are what decide whether the last row lands above or below the
/// window's bottom edge.
#[must_use]
pub fn default_window_size(cols: u16, rows: u16, cell_width: f32, line_height: f32) -> WindowSize {
    let width = ceil_pixels(f32::from(cols) * cell_width.max(0.0));
    let height = ceil_pixels(f32::from(rows) * line_height.max(0.0)) + chrome_height();
    WindowSize { width: width.max(MIN_WINDOW_EDGE), height: height.max(MIN_WINDOW_EDGE) }
}

/// Shrink `size` to fit inside `display`, keeping the floor every edge has.
///
/// A large `appearance.font_size` can ask for a window taller than the screen;
/// opening it anyway would put the status bar off the bottom of the display
/// rather than off the bottom of the window, which is the same bug one level up.
#[must_use]
pub fn clamp_to_display(size: WindowSize, display: WindowSize) -> WindowSize {
    WindowSize {
        width: size.width.min(display.width.max(MIN_WINDOW_EDGE)),
        height: size.height.min(display.height.max(MIN_WINDOW_EDGE)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_WINDOW_EDGE, STATUS_BAR_HEIGHT, WindowSize, chrome_height, clamp_to_display,
        default_window_size,
    };
    use crate::titlebar::TITLEBAR_HEIGHT;

    // @lat: [[test#GPUI Client Headless Suites#Window chrome geometry#Default window size clears every chrome band]]
    #[test]
    fn default_window_size_clears_every_chrome_band() {
        // The shipped defaults: a 120x36 grid at font size 14 (line height
        // 14 * 1.35 = 18.9, cell width 14 * 0.6 = 8.4).
        let size = default_window_size(120, 36, 8.4, 18.9);
        let grid_height = size.height - chrome_height();
        assert!(
            grid_height >= 36.0 * 18.9,
            "all 36 rows must fit above the chrome: {grid_height} < {}",
            36.0 * 18.9
        );
        assert!(size.width >= 120.0 * 8.4, "all 120 columns must fit: {}", size.width);
        // The bands themselves are what the grid has to clear.
        assert!((chrome_height() - (TITLEBAR_HEIGHT + STATUS_BAR_HEIGHT)).abs() < f32::EPSILON);
        // Float noise must not buy a whole extra pixel: the shipped metrics are
        // 14 * 0.6 and 14 * 1.35, whose products land just either side of a
        // whole pixel in f32.
        let shipped = default_window_size(120, 36, 14.0 * 0.6, 14.0f32.mul_add(1.35, 0.0));
        assert!((shipped.width - 1008.0).abs() < f32::EPSILON, "width was {}", shipped.width);
        assert!(
            (shipped.height - (681.0 + chrome_height())).abs() < f32::EPSILON,
            "height was {}",
            shipped.height
        );
        // A degenerate font metric collapses the grid, not the window.
        let tiny = default_window_size(120, 36, 0.0, -5.0);
        assert!((tiny.width - MIN_WINDOW_EDGE).abs() < f32::EPSILON);
        assert!((tiny.height - MIN_WINDOW_EDGE).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Client Headless Suites#Window chrome geometry#Startup size never exceeds the display]]
    #[test]
    fn startup_size_never_exceeds_the_display() {
        // font_size 72 asks for a window far taller than a 1080p screen.
        let huge = default_window_size(120, 36, 43.2, 97.2);
        let clamped = clamp_to_display(huge, WindowSize { width: 1920.0, height: 1080.0 });
        assert!((clamped.width - 1920.0).abs() < f32::EPSILON);
        assert!((clamped.height - 1080.0).abs() < f32::EPSILON);

        // A window that already fits is left alone.
        let fits = default_window_size(120, 36, 8.4, 18.9);
        assert_eq!(clamp_to_display(fits, WindowSize { width: 1920.0, height: 1080.0 }), fits);

        // A nonsense display report cannot clamp the window below the floor.
        let floored = clamp_to_display(fits, WindowSize { width: 1.0, height: 1.0 });
        assert!((floored.width - MIN_WINDOW_EDGE).abs() < f32::EPSILON);
        assert!((floored.height - MIN_WINDOW_EDGE).abs() < f32::EPSILON);
    }
}
