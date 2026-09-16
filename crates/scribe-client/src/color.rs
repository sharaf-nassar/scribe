//! Terminal cell colours in GPUI's native sRGB space.
//!
//! Keep theme, palette and truecolor values in sRGB from input to paint. GPUI
//! owns the GPU color-space conversion; a CPU linear intermediate would add
//! redundant transfer functions to every visible cell on every repaint.
//! [`TerminalColors`] owns the shared BOLD, INVERSE, HIDDEN and DIM semantics.

use alacritty_terminal_gpui::term::cell::Flags;
use scribe_common::theme::Theme;
use vte::ansi::{Color, NamedColor};

use crate::palette::ColorPalette;

/// Dimming factor applied to foreground when the DIM flag is set.
pub const DIM_FACTOR: f32 = 0.67;

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

/// Apply DIM to sRGB channels, leaving alpha unchanged.
pub fn apply_dim(color: &mut [f32; 4]) {
    for channel in color.get_mut(..3).into_iter().flatten() {
        *channel *= DIM_FACTOR;
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
/// Resolves an alacritty cell's raw colour fields to sRGB RGBA, applying
/// the terminal's bold-bright / INVERSE / HIDDEN / DIM rules once.
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
            default_fg: [0.8, 0.8, 0.8, 1.0],
            default_bright_fg: boost_srgb_brightness([0.8, 0.8, 0.8, 1.0]),
            default_bg: [0.0, 0.0, 0.0, 1.0],
            default_fg_dim: [0.8 * DIM_FACTOR, 0.8 * DIM_FACTOR, 0.8 * DIM_FACTOR, 1.0],
            cursor_color: [0.8, 0.8, 0.8, 1.0],
            selection_bg: [0.25, 0.25, 0.28, 1.0],
            selection_fg: [1.0, 1.0, 1.0, 1.0],
        }
    }

    /// Copy the theme's sRGB colours and precompute its semantic variants.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.default_fg = theme.foreground;
        self.default_bright_fg = boost_srgb_brightness(theme.foreground);
        self.default_bg = theme.background;
        self.cursor_color = theme.cursor;
        self.default_fg_dim = theme.foreground;
        apply_dim(&mut self.default_fg_dim);
        self.palette.override_ansi(&theme.ansi_colors);
        self.selection_bg = theme.selection;
        self.selection_fg = theme.selection_foreground;
    }

    /// Current default background colour in sRGB space.
    pub const fn default_bg(&self) -> [f32; 4] {
        self.default_bg
    }

    /// Current cursor colour in sRGB space.
    pub const fn cursor_color(&self) -> [f32; 4] {
        self.cursor_color
    }

    /// Current selection background colour in sRGB space.
    pub const fn selection_bg(&self) -> [f32; 4] {
        self.selection_bg
    }

    /// Current selection foreground colour in sRGB space.
    pub const fn selection_fg(&self) -> [f32; 4] {
        self.selection_fg
    }

    /// Resolve sRGB foreground and background from raw cell fields.
    ///
    /// Order is significant: BOLD promotion, INVERSE swap, HIDDEN, then DIM.
    /// DIM therefore affects the post-swap/post-HIDDEN foreground only.
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

    /// Distinct sRGB channels make accidental color-space conversions visible.
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

    /// The brightness boost pushes each channel 30 % toward 1.0, leaving alpha.
    #[test]
    fn brightness_boost_pushes_channels_toward_white() {
        let boosted = boost_srgb_brightness([0.0, 0.5, 1.0, 0.7]);
        assert_rgba_eq(boosted, [0.3, 0.5 + 0.5 * 0.30, 1.0, 0.7]);
    }

    /// DIM is an sRGB multiplication and must not change theme alpha.
    #[test]
    fn apply_dim_scales_srgb_without_changing_alpha() {
        let mut c = [0.5, 0.25, 1.0, 0.3];
        apply_dim(&mut c);
        assert_rgba_eq(c, [0.5 * DIM_FACTOR, 0.25 * DIM_FACTOR, DIM_FACTOR, 0.3]);
    }

    /// The default (unthemed) colours match the renderer's neutral 0.8 grey
    /// foreground and black background.
    #[test]
    fn default_colors_match_neutral_renderer_defaults() {
        let colors = TerminalColors::new();
        assert_rgba_eq(colors.default_bg(), [0.0, 0.0, 0.0, 1.0]);
        let fg = colors.resolve_color(Color::Named(NamedColor::Foreground));
        assert_rgba_eq(fg, [0.8, 0.8, 0.8, 1.0]);
    }

    /// Semantic colours stay in sRGB after a theme reload, including alpha.
    #[test]
    fn theme_drives_semantic_color_resolution() {
        let mut colors = TerminalColors::new();
        let mut theme = test_theme();
        colors.set_theme(&theme);
        theme.foreground[3] = 0.4;
        theme.background[3] = 0.7;
        theme.ansi_colors[0] = [0.3, 0.2, 0.1, 0.6];
        colors.set_theme(&theme);
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::Foreground)),
            theme.foreground,
        );
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::Background)),
            theme.background,
        );
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::BrightForeground)),
            boost_srgb_brightness(theme.foreground),
        );
        assert_rgba_eq(
            colors.resolve_color(Color::Named(NamedColor::DimForeground)),
            [0.5 * DIM_FACTOR, 0.6 * DIM_FACTOR, 0.7 * DIM_FACTOR, 0.4],
        );
        assert_rgba_eq(colors.resolve_color(Color::Indexed(0)), theme.ansi_colors[0]);
        assert_rgba_eq(colors.default_bg(), theme.background);
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
        assert_rgba_eq(fg, boost_srgb_brightness([0.5, 0.6, 0.7, 1.0]));
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

    /// DIM scales only the foreground's sRGB channels.
    #[test]
    fn dim_flag_dims_foreground_only() {
        let colors = TerminalColors::new();
        let (fg, bg) = colors.resolve_cell_colors(Color::Indexed(1), Color::Indexed(4), Flags::DIM);
        let mut expected_fg = colors.resolve_color(Color::Indexed(1));
        apply_dim(&mut expected_fg);
        assert_rgba_eq(fg, expected_fg);
        assert_rgba_eq(bg, colors.resolve_color(Color::Indexed(4)));
    }

    // @lat: [[test#Test Harness#GPUI Client Headless Suites#Cell-accurate paint path#Combined color flags preserve order and alpha]]
    #[test]
    fn combined_color_flags_preserve_order_and_alpha() {
        let mut theme = test_theme();
        theme.foreground[3] = 0.4;
        theme.background[3] = 0.7;
        let mut colors = TerminalColors::new();
        colors.set_theme(&theme);
        let bright = boost_srgb_brightness(theme.foreground);
        let dim_bright =
            [bright[0] * DIM_FACTOR, bright[1] * DIM_FACTOR, bright[2] * DIM_FACTOR, 0.4];
        let dim_background = [0.02 * DIM_FACTOR, 0.03 * DIM_FACTOR, 0.04 * DIM_FACTOR, 0.7];
        for (flags, expected_fg, expected_bg) in [
            (Flags::BOLD | Flags::DIM, dim_bright, theme.background),
            (Flags::INVERSE | Flags::DIM, dim_background, theme.foreground),
            (Flags::HIDDEN | Flags::DIM, dim_background, theme.background),
            (Flags::BOLD | Flags::INVERSE | Flags::HIDDEN | Flags::DIM, dim_bright, bright),
        ] {
            let (fg, bg) = colors.resolve_cell_colors(
                Color::Named(NamedColor::Foreground),
                Color::Named(NamedColor::Background),
                flags,
            );
            assert_rgba_eq(fg, expected_fg);
            assert_rgba_eq(bg, expected_bg);
        }
    }

    fn color_resolution_sample(colors: &TerminalColors, cells: &[(Color, Color, Flags)]) -> u128 {
        use std::{hint::black_box, time::Instant};

        const PASSES: usize = 64;
        let start = Instant::now();
        for _ in 0..PASSES {
            for &(fg, bg, flags) in black_box(cells) {
                black_box(black_box(colors).resolve_cell_colors(fg, bg, flags));
            }
        }
        start.elapsed().as_nanos() / (cells.len() * PASSES) as u128
    }

    /// Repeatable optimized hot-path measurement, not a wall-clock CI assertion.
    #[test]
    #[ignore = "manual color-resolution benchmark; run with package opt-level=3 and --nocapture"]
    fn benchmark_cell_color_resolution() {
        use std::{hint::black_box, io::Write as _};

        const CELLS: usize = 4096;
        let mut colors = TerminalColors::new();
        colors.set_theme(&test_theme());
        for workload in ["default", "indexed", "truecolor"] {
            let cells: Vec<_> = (0..CELLS)
                .map(|i| {
                    let v = u8::try_from(i % 256).unwrap();
                    let (fg, bg) = match workload {
                        "default" => (
                            Color::Named(NamedColor::Foreground),
                            Color::Named(NamedColor::Background),
                        ),
                        "indexed" => (Color::Indexed(v), Color::Indexed(v.wrapping_add(73))),
                        _ => (
                            Color::Spec(Rgb {
                                r: v,
                                g: v.wrapping_add(91),
                                b: v.wrapping_add(173),
                            }),
                            Color::Spec(Rgb { r: v.wrapping_add(37), g: v, b: v.wrapping_add(19) }),
                        ),
                    };
                    let flags = [Flags::empty(), Flags::BOLD, Flags::DIM, Flags::INVERSE][i % 4];
                    (fg, bg, flags)
                })
                .collect();
            black_box(color_resolution_sample(&colors, &cells));
            let mut samples: Vec<_> =
                (0..7).map(|_| color_resolution_sample(&colors, &cells)).collect();
            samples.sort_unstable();
            writeln!(
                std::io::stdout(),
                "{workload}: median_ns_per_cell={} samples={samples:?}",
                samples[3]
            )
            .unwrap();
        }
    }

    /// Selection colours track the theme.
    #[test]
    fn selection_colors_track_theme() {
        let mut colors = TerminalColors::new();
        colors.set_theme(&test_theme());
        assert_rgba_eq(colors.selection_bg(), [0.2, 0.2, 0.25, 1.0]);
        assert_rgba_eq(colors.selection_fg(), [1.0, 1.0, 1.0, 1.0]);
        assert_rgba_eq(colors.cursor_color(), [0.9, 0.9, 0.9, 1.0]);
    }
}
