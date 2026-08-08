//! Runtime font zoom for the terminal grid.
//!
//! Zoom is a per-window integer point delta applied on top of the configured
//! font size, clamped to `[-7, +7]` so the grid never collapses or explodes.
//! In/out step by one point; reset returns to the configured size. The GPUI
//! shell reads [`ZoomState::effective_font_size`] to rebuild the font atlas and
//! re-lay the grid when the level changes; the math is isolated here so the
//! clamping and floor can be tested without a window.

/// Minimum zoom level in font-size points.
const ZOOM_MIN: i8 = -7;

/// Maximum zoom level in font-size points.
const ZOOM_MAX: i8 = 7;

/// Smallest font size (points) the terminal will render at, matching the
/// legacy client's floor so extreme zoom-out still produces legible cells.
const MIN_FONT_SIZE: f32 = 6.0;

/// Runtime zoom state: a signed point delta over the configured font size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZoomState {
    level: i8,
}

impl ZoomState {
    /// Create a zoom state at the neutral (configured) level.
    #[must_use]
    pub fn new() -> Self {
        Self { level: 0 }
    }

    /// A zoom state at a persisted level, clamped to `[ZOOM_MIN, ZOOM_MAX]`.
    ///
    /// The level survives a quit in the window's geometry record, which is a
    /// plain TOML file a user (or a truncated write) can put any `i8` in, so
    /// the range is re-imposed on the way back in rather than trusted.
    #[must_use]
    pub fn at_level(level: i8) -> Self {
        Self { level: level.clamp(ZOOM_MIN, ZOOM_MAX) }
    }

    /// Current zoom level in font-size points.
    #[must_use]
    pub fn level(self) -> i8 {
        self.level
    }

    /// Increase the zoom by one point, saturating at [`ZOOM_MAX`].
    pub fn zoom_in(&mut self) {
        self.step(1);
    }

    /// Decrease the zoom by one point, saturating at [`ZOOM_MIN`].
    pub fn zoom_out(&mut self) {
        self.step(-1);
    }

    /// Reset the zoom to the configured font size.
    pub fn reset(&mut self) {
        self.level = 0;
    }

    /// The effective font size for `base_font_size` at the current zoom level,
    /// floored at [`MIN_FONT_SIZE`].
    #[must_use]
    pub fn effective_font_size(self, base_font_size: f32) -> f32 {
        (base_font_size + f32::from(self.level)).max(MIN_FONT_SIZE)
    }

    fn step(&mut self, delta: i8) {
        self.level = self.level.saturating_add(delta).clamp(ZOOM_MIN, ZOOM_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#GPUI Font Zoom#Zoom steps clamp to the point range]]
    #[test]
    fn zoom_steps_clamp_to_point_range() {
        let mut z = ZoomState::new();
        for _ in 0..20 {
            z.zoom_in();
        }
        assert_eq!(z.level(), ZOOM_MAX);
        for _ in 0..40 {
            z.zoom_out();
        }
        assert_eq!(z.level(), ZOOM_MIN);
    }

    // @lat: [[test#GPUI Font Zoom#A restored level is clamped, not trusted]]
    #[test]
    fn restored_level_is_clamped() {
        assert_eq!(ZoomState::at_level(0), ZoomState::new());
        assert_eq!(ZoomState::at_level(-3).level(), -3);
        // A hand-edited or corrupt record cannot escape the range the live
        // steps saturate at.
        assert_eq!(ZoomState::at_level(120).level(), ZOOM_MAX);
        assert_eq!(ZoomState::at_level(i8::MIN).level(), ZOOM_MIN);
        // And what it restores is a delta over the configured size, so a
        // config edit rebases it rather than being overridden by it.
        assert!((ZoomState::at_level(-2).effective_font_size(20.0) - 18.0).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Font Zoom#Reset returns to the configured size]]
    #[test]
    fn reset_returns_to_configured_size() {
        let mut z = ZoomState::new();
        z.zoom_in();
        z.zoom_in();
        assert_eq!(z.level(), 2);
        z.reset();
        assert_eq!(z.level(), 0);
        assert!((z.effective_font_size(14.0) - 14.0).abs() < f32::EPSILON);
    }

    // @lat: [[test#GPUI Font Zoom#Effective size applies the delta and honors the floor]]
    #[test]
    fn effective_size_applies_delta_and_honors_floor() {
        let mut z = ZoomState::new();
        z.zoom_in();
        assert!((z.effective_font_size(14.0) - 15.0).abs() < f32::EPSILON);
        z.reset();
        z.zoom_out();
        z.zoom_out();
        // 8pt base minus 2 zoom = 6pt, exactly the floor.
        assert!((z.effective_font_size(8.0) - 6.0).abs() < f32::EPSILON);
        // 6pt base minus 7 zoom would be negative; floored to 6.
        for _ in 0..5 {
            z.zoom_out();
        }
        assert!((z.effective_font_size(6.0) - MIN_FONT_SIZE).abs() < f32::EPSILON);
    }
}
