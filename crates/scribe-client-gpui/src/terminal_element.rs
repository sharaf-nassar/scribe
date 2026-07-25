//! GPUI paint path for a display-only terminal [`Content`](crate::terminal::Content) snapshot.

use gpui::{Rgba, div, prelude::*, px};
use scribe_common::config::AppearanceConfig;

use crate::terminal::Content;

/// Smallest font size the grid will paint at, so a bad `appearance.font_size`
/// edit (0, negative) can never collapse the grid to nothing.
const MIN_FONT_SIZE: f32 = 6.0;

/// Row height as a multiple of the font size, before `appearance.line_padding`
/// is added. Matches the ~1.35 leading the legacy atlas used for its cell box.
const LINE_HEIGHT_RATIO: f32 = 1.35;

/// Cell advance as a multiple of the font size for the monospace grid. The
/// legacy renderer measured this from the shaped glyph; the display-only spike
/// approximates it so a live font-size edit still moves the reported cell size.
const CELL_WIDTH_RATIO: f32 = 0.6;

/// The font metrics the terminal grid paints with.
///
/// Derived from the live `[appearance]` config on every config reload, so a
/// saved `font` / `font_size` / `line_padding` edit repaints the grid without a
/// restart instead of staying frozen at the value read during startup.
#[derive(Debug, Clone, PartialEq)]
pub struct GridFont {
    /// Font family passed to GPUI's text system.
    pub family: String,
    /// Glyph size in pixels, clamped to at least [`MIN_FONT_SIZE`].
    pub size: f32,
    /// Row height in pixels, including `appearance.line_padding`.
    pub line_height: f32,
}

impl GridFont {
    /// Derive the paint metrics from the live appearance config.
    #[must_use]
    pub fn from_appearance(appearance: &AppearanceConfig) -> Self {
        let size = appearance.font_size.max(MIN_FONT_SIZE);
        Self {
            family: appearance.font.clone(),
            size,
            line_height: size.mul_add(LINE_HEIGHT_RATIO, f32::from(appearance.line_padding)),
        }
    }

    /// The per-cell advance width reported to the server in `TerminalSize`.
    #[must_use]
    pub fn cell_width(&self) -> f32 {
        self.size * CELL_WIDTH_RATIO
    }
}

impl Default for GridFont {
    fn default() -> Self {
        Self::from_appearance(&AppearanceConfig::default())
    }
}

/// The theme colours the terminal grid paints with.
///
/// Derived on every render from the live theme, so a saved `theme` edit
/// repaints the grid instead of leaving it on a hardcoded palette. `background`
/// already carries the `appearance.opacity` alpha; `foreground` is deliberately
/// left at the theme's own alpha so glyphs stay readable through a translucent
/// window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridColors {
    /// Grid background, alpha-scaled by `appearance.opacity`.
    pub background: Rgba,
    /// Default glyph colour, never scaled by opacity.
    pub foreground: Rgba,
}

/// Paints the current terminal grid with fixed-width rows.
pub struct TerminalElement {
    content: Content,
    font: GridFont,
    colors: GridColors,
}

impl TerminalElement {
    /// Captures one stable terminal snapshot for this render pass, painted with
    /// the font metrics and theme colours resolved from the live config.
    pub const fn new(content: Content, font: GridFont, colors: GridColors) -> Self {
        Self { content, font, colors }
    }

    /// Builds the GPUI element tree for the visible terminal grid.
    pub fn paint(self) -> impl IntoElement {
        let line_height = px(self.font.line_height);
        div()
            .size_full()
            .overflow_hidden()
            .bg(self.colors.background)
            .text_color(self.colors.foreground)
            .font_family(self.font.family)
            .text_size(px(self.font.size))
            .line_height(line_height)
            .child(
                div().flex().flex_col().children(
                    self.content.rows.into_iter().map(|row| div().h(line_height).child(row)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{CELL_WIDTH_RATIO, GridFont, MIN_FONT_SIZE};
    use scribe_common::config::AppearanceConfig;

    // @lat: [[test#GPUI Client Headless Suites#Config live reload#Grid font tracks the live appearance config]]
    #[test]
    fn grid_font_tracks_appearance_edits() {
        let mut appearance = AppearanceConfig {
            font: "Fira Code".to_owned(),
            font_size: 18.0,
            line_padding: 4,
            ..AppearanceConfig::default()
        };
        let font = GridFont::from_appearance(&appearance);
        assert_eq!(font.family, "Fira Code");
        assert!((font.size - 18.0).abs() < f32::EPSILON);
        assert!((font.line_height - 18.0f32.mul_add(1.35, 4.0)).abs() < f32::EPSILON);
        assert!((font.cell_width() - 18.0 * CELL_WIDTH_RATIO).abs() < f32::EPSILON);

        // A nonsense size is clamped rather than collapsing the grid.
        appearance.font_size = 0.0;
        let clamped = GridFont::from_appearance(&appearance);
        assert!((clamped.size - MIN_FONT_SIZE).abs() < f32::EPSILON);
    }
}
