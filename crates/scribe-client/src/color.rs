//! Terminal cell colour semantics, ported from the retired renderer.
//!
//! This module owns the pure colour logic the GPUI paint path needs: sRGB↔
//! linear conversions, the DIM (0.67) round-trip, bold→bright promotion, the
//! `BrightForeground` brightness boost, and per-cell foreground/background
//! resolution (bold-bright, INVERSE, HIDDEN, DIM). [`TerminalColors`] holds
//! the theme-derived default colours plus the xterm-256 [`ColorPalette`] and
//! resolves an alacritty cell's raw colour fields to linear RGBA, byte-for-
//! byte identical to the legacy renderer.

use alacritty_terminal_gpui::term::cell::Flags;
use scribe_common::theme::Theme;
use vte::ansi::{Color, NamedColor};

use crate::palette::ColorPalette;

/// Dimming factor applied to foreground when the DIM flag is set.
pub const DIM_FACTOR: f32 = 0.67;

/// Convert a single sRGB channel to linear space.
#[inline]
fn srgb_channel_to_linear(s: f32) -> f32 {
    if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
}

/// Convert a single linear channel to sRGB space.
///
/// This is the inverse of [`srgb_channel_to_linear`].
#[inline]
fn linear_to_srgb_channel(l: f32) -> f32 {
    if l <= 0.003_130_8 { l * 12.92 } else { l.powf(1.0 / 2.4).mul_add(1.055, -0.055) }
}

/// Convert an sRGB `[f32; 4]` colour to linear space (alpha unchanged).
///
/// Use this for any sRGB colors (e.g. theme colors) that will be passed to
/// the GPU pipeline, which expects linear colors.
pub fn srgb_to_linear_rgba(c: [f32; 4]) -> [f32; 4] {
    [
        srgb_channel_to_linear(c.first().copied().unwrap_or(0.0)),
        srgb_channel_to_linear(c.get(1).copied().unwrap_or(0.0)),
        srgb_channel_to_linear(c.get(2).copied().unwrap_or(0.0)),
        c.get(3).copied().unwrap_or(1.0),
    ]
}

/// Convert a linear `[f32; 4]` colour back to sRGB space (alpha unchanged).
///
/// The inverse of [`srgb_to_linear_rgba`]. The legacy renderer kept everything
/// linear because its wgpu pipeline wrote into an sRGB framebuffer; GPUI's
/// `Rgba` is already sRGB, so the paint path converts back at the boundary
/// rather than duplicating the SGR resolution rules in a second colour space.
pub fn linear_to_srgb_rgba(c: [f32; 4]) -> [f32; 4] {
    [
        linear_to_srgb_channel(c.first().copied().unwrap_or(0.0)),
        linear_to_srgb_channel(c.get(1).copied().unwrap_or(0.0)),
        linear_to_srgb_channel(c.get(2).copied().unwrap_or(0.0)),
        c.get(3).copied().unwrap_or(1.0),
    ]
}

/// Boost an sRGB colour toward full brightness for the bold-bright foreground.
///
/// Each channel is pushed 30 % of the way toward 1.0, so dim themes gain a
/// noticeable bump while themes near white stay clamped.
pub fn boost_srgb_brightness(srgb: [f32; 4]) -> [f32; 4] {
    const FACTOR: f32 = 0.30;
    [
        srgb[0] + (1.0 - srgb[0]) * FACTOR,
        srgb[1] + (1.0 - srgb[1]) * FACTOR,
        srgb[2] + (1.0 - srgb[2]) * FACTOR,
        srgb[3],
    ]
}

/// Apply the DIM effect in sRGB space, then convert back to linear.
///
/// Terminal convention applies DIM by multiplying sRGB channel values by
/// [`DIM_FACTOR`]. Because our pipeline stores linear colours, we round-trip
/// through sRGB so the perceptual result matches other terminal emulators.
pub fn apply_dim(color: &mut [f32; 4]) {
    for c in color.get_mut(..3).into_iter().flatten() {
        let srgb = linear_to_srgb_channel(*c);
        *c = srgb_channel_to_linear(srgb * DIM_FACTOR);
    }
}

/// Map a foreground colour to its bright variant when the BOLD flag is set.
///
/// Standard terminal behaviour: ANSI colours 0-7 (named or indexed) are
/// promoted to their bright equivalents 8-15, and the semantic `Foreground`
/// is promoted to `BrightForeground`. RGB / 256-colour values and already-
/// bright colours are returned unchanged.
pub fn bold_to_bright(color: Color) -> Color {
    match color {
        Color::Named(named) => Color::Named(match named {
            NamedColor::Black => NamedColor::BrightBlack,
            NamedColor::Red => NamedColor::BrightRed,
            NamedColor::Green => NamedColor::BrightGreen,
            NamedColor::Yellow => NamedColor::BrightYellow,
            NamedColor::Blue => NamedColor::BrightBlue,
            NamedColor::Magenta => NamedColor::BrightMagenta,
            NamedColor::Cyan => NamedColor::BrightCyan,
            NamedColor::White => NamedColor::BrightWhite,
            NamedColor::Foreground => NamedColor::BrightForeground,
            other => other,
        }),
        Color::Indexed(idx @ 0..=7) => Color::Indexed(idx + 8),
        other => other,
    }
}

/// Theme-derived default colours plus the xterm-256 palette.
///
/// Resolves an alacritty cell's raw colour fields to linear RGBA, applying
/// the same bold-bright / INVERSE / HIDDEN / DIM rules as the legacy
/// `TerminalRenderer`.
pub struct TerminalColors {
    palette: ColorPalette,
    default_fg: [f32; 4],
    default_bright_fg: [f32; 4],
    default_bg: [f32; 4],
    default_fg_dim: [f32; 4],
    cursor_color: [f32; 4],
    selection_bg: [f32; 4],
    selection_fg: [f32; 4],
}

impl TerminalColors {
    /// Build the default colours (matching the renderer's neutral defaults
    /// before any theme is applied).
    pub fn new() -> Self {
        Self {
            palette: ColorPalette::new(),
            default_fg: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
            default_bright_fg: srgb_to_linear_rgba(boost_srgb_brightness([0.8, 0.8, 0.8, 1.0])),
            default_bg: srgb_to_linear_rgba([0.0, 0.0, 0.0, 1.0]),
            default_fg_dim: {
                // Apply DIM in sRGB space: multiply the sRGB value by DIM_FACTOR,
                // then convert to linear for the GPU pipeline.
                let srgb = [0.8_f32, 0.8, 0.8, 1.0];
                [
                    srgb_channel_to_linear(srgb[0] * DIM_FACTOR),
                    srgb_channel_to_linear(srgb[1] * DIM_FACTOR),
                    srgb_channel_to_linear(srgb[2] * DIM_FACTOR),
                    srgb[3],
                ]
            },
            cursor_color: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
            selection_bg: srgb_to_linear_rgba([0.25, 0.25, 0.28, 1.0]),
            selection_fg: srgb_to_linear_rgba([1.0, 1.0, 1.0, 1.0]),
        }
    }

    /// Apply a theme, updating the palette and default colours.
    ///
    /// Theme colors are sRGB; we convert to linear for the GPU pipeline (the
    /// sRGB framebuffer applies the inverse transform on output).
    pub fn set_theme(&mut self, theme: &Theme) {
        self.default_fg = srgb_to_linear_rgba(theme.foreground);
        self.default_bright_fg = srgb_to_linear_rgba(boost_srgb_brightness(theme.foreground));
        self.default_bg = srgb_to_linear_rgba(theme.background);
        self.cursor_color = srgb_to_linear_rgba(theme.cursor);
        // Apply DIM in sRGB space: use the raw sRGB theme foreground values,
        // multiply by DIM_FACTOR, then convert to linear for the GPU pipeline.
        let srgb_fg = theme.foreground;
        self.default_fg_dim = [
            srgb_channel_to_linear(srgb_fg.first().copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_channel_to_linear(srgb_fg.get(1).copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_channel_to_linear(srgb_fg.get(2).copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_fg.get(3).copied().unwrap_or(1.0),
        ];
        let mut linear_ansi = [[0.0_f32; 4]; 16];
        for (i, color) in theme.ansi_colors.iter().enumerate() {
            if let Some(dest) = linear_ansi.get_mut(i) {
                *dest = srgb_to_linear_rgba(*color);
            }
        }
        self.palette.override_ansi(&linear_ansi);
        self.selection_bg = srgb_to_linear_rgba(theme.selection);
        self.selection_fg = srgb_to_linear_rgba(theme.selection_foreground);
    }

    /// Current default background colour (linear space; usable as clear colour).
    pub const fn default_bg(&self) -> [f32; 4] {
        self.default_bg
    }

    /// Current cursor colour (linear space).
    pub const fn cursor_color(&self) -> [f32; 4] {
        self.cursor_color
    }

    /// Current selection background colour (linear space).
    pub const fn selection_bg(&self) -> [f32; 4] {
        self.selection_bg
    }

    /// Current selection foreground colour (linear space).
    pub const fn selection_fg(&self) -> [f32; 4] {
        self.selection_fg
    }

    /// Resolve foreground and background colours from raw cell fields,
    /// applying BOLD-bright, INVERSE, HIDDEN, and DIM flags.
    pub fn resolve_cell_colors(
        &self,
        fg_color: Color,
        bg_color: Color,
        flags: Flags,
    ) -> ([f32; 4], [f32; 4]) {
        let effective_fg =
            if flags.contains(Flags::BOLD) { bold_to_bright(fg_color) } else { fg_color };
        let mut fg = self.resolve_color(effective_fg);
        let mut bg = self.resolve_color(bg_color);

        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        if flags.contains(Flags::DIM) {
            apply_dim(&mut fg);
        }

        (fg, bg)
    }

    /// Resolve foreground and background colours for one cell in sRGB space,
    /// ready to hand straight to GPUI.
    ///
    /// Identical rules to [`Self::resolve_cell_colors`] — the SGR semantics
    /// live in exactly one place — with the result converted out of the linear
    /// space the legacy wgpu pipeline needed.
    pub fn resolve_cell_colors_srgb(
        &self,
        fg_color: Color,
        bg_color: Color,
        flags: Flags,
    ) -> ([f32; 4], [f32; 4]) {
        let (fg, bg) = self.resolve_cell_colors(fg_color, bg_color, flags);
        (linear_to_srgb_rgba(fg), linear_to_srgb_rgba(bg))
    }

    /// Resolve an alacritty colour to RGBA floats, using sensible defaults for
    /// semantic colours (Foreground, Background, etc.).
    pub fn resolve_color(&self, color: Color) -> [f32; 4] {
        match color {
            Color::Named(NamedColor::Foreground | NamedColor::Cursor) => self.default_fg,
            Color::Named(NamedColor::BrightForeground) => self.default_bright_fg,
            Color::Named(NamedColor::Background) => self.default_bg,
            Color::Named(NamedColor::DimForeground) => self.default_fg_dim,
            other => self.palette.resolve(other),
        }
    }
}

impl Default for TerminalColors {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use scribe_common::theme::minimal_dark;
    use vte::ansi::Rgb;

    use super::*;
    use crate::assert_rgba_eq;

    /// A theme fixture with distinct sRGB channels so byte-exact conversions
    /// are easy to assert, built by overriding a preset's colour fields.
    fn test_theme() -> Theme {
        let mut theme = minimal_dark();
        theme.foreground = [0.5, 0.6, 0.7, 1.0];
        theme.background = [0.02, 0.03, 0.04, 1.0];
        theme.cursor = [0.9, 0.9, 0.9, 1.0];
        theme.selection = [0.2, 0.2, 0.25, 1.0];
        theme.selection_foreground = [1.0, 1.0, 1.0, 1.0];
        theme.ansi_colors = [[0.11, 0.22, 0.33, 1.0]; 16];
        theme
    }

    /// `bold_to_bright` promotes standard named and indexed ANSI colours and
    /// the semantic foreground, leaving true-colour values untouched.
    #[test]
    fn bold_to_bright_promotes_standard_colors_only() {
        assert_eq!(
            bold_to_bright(Color::Named(NamedColor::Red)),
            Color::Named(NamedColor::BrightRed)
        );
        assert_eq!(
            bold_to_bright(Color::Named(NamedColor::Foreground)),
            Color::Named(NamedColor::BrightForeground)
        );
        assert_eq!(bold_to_bright(Color::Indexed(3)), Color::Indexed(11));
        // Already-bright and true colour pass through.
        assert_eq!(bold_to_bright(Color::Indexed(12)), Color::Indexed(12));
        let spec = Color::Spec(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(bold_to_bright(spec), spec);
    }

    /// sRGB→linear conversion matches the piecewise sRGB transfer function at
    /// a mid-tone value, byte-for-byte.
    #[test]
    fn srgb_to_linear_matches_transfer_function() {
        let c = srgb_to_linear_rgba([0.5, 0.5, 0.5, 0.3]);
        let expected = ((0.5_f32 + 0.055) / 1.055).powf(2.4);
        assert_rgba_eq(c, [expected, expected, expected, 0.3]);
    }

    /// The brightness boost pushes each channel 30 % toward 1.0, leaving alpha.
    #[test]
    fn brightness_boost_pushes_channels_toward_white() {
        let boosted = boost_srgb_brightness([0.0, 0.5, 1.0, 0.7]);
        assert_rgba_eq(boosted, [0.3, 0.5 + 0.5 * 0.30, 1.0, 0.7]);
    }

    /// DIM round-trips through sRGB: a linear channel is converted to sRGB,
    /// multiplied by 0.67, and converted back.
    #[test]
    fn apply_dim_round_trips_through_srgb() {
        let mut c = [0.5, 0.5, 0.5, 1.0];
        apply_dim(&mut c);
        let srgb = linear_to_srgb_channel(0.5);
        let expected = srgb_channel_to_linear(srgb * DIM_FACTOR);
        assert_rgba_eq(c, [expected, expected, expected, 1.0]);
    }

    /// The default (unthemed) colours match the renderer's neutral 0.8 grey
    /// foreground and black background.
    #[test]
    fn default_colors_match_neutral_renderer_defaults() {
        let colors = TerminalColors::new();
        assert_rgba_eq(colors.default_bg(), srgb_to_linear_rgba([0.0, 0.0, 0.0, 1.0]));
        let fg = colors.resolve_color(Color::Named(NamedColor::Foreground));
        assert_rgba_eq(fg, srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]));
    }

    /// After a theme is applied, semantic Foreground/Background resolve to the
    /// linearised theme colours and `BrightForeground` to the boosted foreground.
    #[test]
    fn theme_drives_semantic_color_resolution() {
        let mut colors = TerminalColors::new();
        colors.set_theme(&test_theme());
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::Foreground)),
            srgb_to_linear_rgba([0.5, 0.6, 0.7, 1.0]),
        );
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::Background)),
            srgb_to_linear_rgba([0.02, 0.03, 0.04, 1.0]),
        );
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::BrightForeground)),
            srgb_to_linear_rgba(boost_srgb_brightness([0.5, 0.6, 0.7, 1.0])),
        );
        assert_rgba_eq(colors.default_bg(), srgb_to_linear_rgba([0.02, 0.03, 0.04, 1.0]));
    }

    /// A BOLD cell with the semantic foreground resolves to the boosted
    /// bright foreground; the background is unaffected.
    #[test]
    fn bold_cell_uses_bright_foreground() {
        let mut colors = TerminalColors::new();
        colors.set_theme(&test_theme());
        let (fg, _bg) = colors.resolve_cell_colors(
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            Flags::BOLD,
        );
        assert_rgba_eq(fg, srgb_to_linear_rgba(boost_srgb_brightness([0.5, 0.6, 0.7, 1.0])));
    }

    /// INVERSE swaps foreground and background before other adjustments.
    #[test]
    fn inverse_flag_swaps_fg_and_bg() {
        let colors = TerminalColors::new();
        let fg_in = Color::Indexed(1);
        let bg_in = Color::Indexed(4);
        let (fg, bg) = colors.resolve_cell_colors(fg_in, bg_in, Flags::INVERSE);
        assert_rgba_eq(fg, colors.resolve_color(bg_in));
        assert_rgba_eq(bg, colors.resolve_color(fg_in));
    }

    /// HIDDEN forces the foreground to equal the background.
    #[test]
    fn hidden_flag_masks_foreground() {
        let colors = TerminalColors::new();
        let (fg, bg) =
            colors.resolve_cell_colors(Color::Indexed(1), Color::Indexed(4), Flags::HIDDEN);
        assert_rgba_eq(fg, bg);
    }

    /// DIM dims the foreground via the sRGB round-trip, leaving background.
    #[test]
    fn dim_flag_dims_foreground_only() {
        let colors = TerminalColors::new();
        let (fg, bg) = colors.resolve_cell_colors(Color::Indexed(1), Color::Indexed(4), Flags::DIM);
        let mut expected_fg = colors.resolve_color(Color::Indexed(1));
        apply_dim(&mut expected_fg);
        assert_rgba_eq(fg, expected_fg);
        assert_rgba_eq(bg, colors.resolve_color(Color::Indexed(4)));
    }

    /// Selection colours track the theme.
    #[test]
    fn selection_colors_track_theme() {
        let mut colors = TerminalColors::new();
        colors.set_theme(&test_theme());
        assert_rgba_eq(colors.selection_bg(), srgb_to_linear_rgba([0.2, 0.2, 0.25, 1.0]));
        assert_rgba_eq(colors.selection_fg(), srgb_to_linear_rgba([1.0, 1.0, 1.0, 1.0]));
        assert_rgba_eq(colors.cursor_color(), srgb_to_linear_rgba([0.9, 0.9, 0.9, 1.0]));
    }
}
