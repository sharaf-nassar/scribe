use std::borrow::Cow;

use crate::error::ScribeError;

#[path = "theme_community_presets.rs"]
mod community_presets;

/// Curated built-in theme preset names.
const CURATED_NAMES: &[&str] =
    &["minimal-dark", "tokyo-night", "catppuccin-mocha", "dracula", "solarized-dark"];

/// Return the full list of available preset names (curated + community).
#[must_use]
pub fn all_preset_names() -> Vec<&'static str> {
    let mut names = Vec::with_capacity(CURATED_NAMES.len() + community_presets::NAMES.len());
    names.extend_from_slice(CURATED_NAMES);
    names.extend_from_slice(community_presets::NAMES);
    names
}

/// Chrome (non-terminal UI) colors derived from a terminal theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeColors {
    pub tab_bar_bg: [f32; 4],
    pub tab_bar_active_bg: [f32; 4],
    pub tab_text: [f32; 4],
    pub tab_text_active: [f32; 4],
    pub tab_separator: [f32; 4],
    pub status_bar_bg: [f32; 4],
    pub status_bar_text: [f32; 4],
    pub divider: [f32; 4],
    pub accent: [f32; 4],
    pub scrollbar: [f32; 4],
}

/// A complete terminal color theme including chrome (UI) colors.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: Cow<'static, str>,
    pub foreground: [f32; 4],
    pub background: [f32; 4],
    pub cursor: [f32; 4],
    pub cursor_accent: [f32; 4],
    pub selection: [f32; 4],
    pub selection_foreground: [f32; 4],
    pub ansi_colors: [[f32; 4]; 16],
    pub chrome: ChromeColors,
}

/// Input parameters for constructing a `Theme` via [`Theme::from_colors`].
#[derive(Debug, Clone)]
pub struct ThemeColors {
    pub name: Cow<'static, str>,
    pub foreground: [f32; 4],
    pub background: [f32; 4],
    pub cursor: [f32; 4],
    pub cursor_accent: [f32; 4],
    pub selection: [f32; 4],
    pub selection_foreground: [f32; 4],
    pub ansi_colors: [[f32; 4]; 16],
}

impl Theme {
    /// Construct a `Theme` from its component colors, automatically deriving chrome colors.
    #[must_use]
    pub fn from_colors(colors: &ThemeColors) -> Self {
        let chrome = Self::derive_chrome(colors.foreground, colors.background, &colors.ansi_colors);
        Self {
            name: colors.name.clone(),
            foreground: colors.foreground,
            background: colors.background,
            cursor: colors.cursor,
            cursor_accent: colors.cursor_accent,
            selection: colors.selection,
            selection_foreground: colors.selection_foreground,
            ansi_colors: colors.ansi_colors,
            chrome,
        }
    }

    /// Derive chrome colors from the terminal's foreground, background, and ANSI palette.
    fn derive_chrome(
        foreground: [f32; 4],
        background: [f32; 4],
        ansi_colors: &[[f32; 4]; 16],
    ) -> ChromeColors {
        let tab_bar_bg = lighten(background, 0.06);
        let tab_bar_active_bg = background;
        let tab_text = with_alpha(foreground, 0.45);
        let tab_text_active = foreground;
        let tab_separator = with_alpha(foreground, 0.12);
        let status_bar_bg = tab_bar_bg;
        let status_bar_text = with_alpha(foreground, 0.5);
        let divider = with_alpha(foreground, 0.08);
        // ANSI blue is index 4
        let accent = ansi_colors.get(4).copied().unwrap_or(foreground);
        let scrollbar = with_alpha(foreground, 0.4);

        ChromeColors {
            tab_bar_bg,
            tab_bar_active_bg,
            tab_text,
            tab_text_active,
            tab_separator,
            status_bar_bg,
            status_bar_text,
            divider,
            accent,
            scrollbar,
        }
    }
}

/// Look up a built-in theme preset by name (case-insensitive).
pub fn resolve_preset(name: &str) -> Option<Theme> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "minimal-dark" => Some(minimal_dark()),
        "tokyo-night" => Some(tokyo_night()),
        "catppuccin-mocha" => Some(catppuccin_mocha()),
        "dracula" => Some(dracula()),
        "solarized-dark" => Some(solarized_dark()),
        _ => community_presets::lookup(&lower),
    }
}

/// Parse a `#rrggbb` hex string into an `[f32; 4]` RGBA color with alpha 1.0.
pub fn hex_to_rgba(hex: &str) -> Result<[f32; 4], ScribeError> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);

    if hex.len() != 6 {
        return Err(ScribeError::ThemeParse {
            reason: format!("expected 6 hex digits, got {}", hex.len()),
        });
    }

    let red = parse_hex_channel(hex, 0..2, "red")?;
    let green = parse_hex_channel(hex, 2..4, "green")?;
    let blue = parse_hex_channel(hex, 4..6, "blue")?;

    Ok([f32::from(red) / 255.0, f32::from(green) / 255.0, f32::from(blue) / 255.0, 1.0])
}

/// Convert an `[f32; 4]` RGBA color back to a `#rrggbb` hex string.
///
/// Alpha is ignored since hex notation does not encode it.
#[must_use]
pub fn rgba_to_hex(color: [f32; 4]) -> String {
    let red = channel_to_u8(color.first().copied().unwrap_or(0.0));
    let green = channel_to_u8(color.get(1).copied().unwrap_or(0.0));
    let blue = channel_to_u8(color.get(2).copied().unwrap_or(0.0));
    format!("#{red:02x}{green:02x}{blue:02x}")
}

// ---------------------------------------------------------------------------
// Preset builders
// ---------------------------------------------------------------------------

/// Minimal Dark -- the default Scribe theme.
#[must_use]
pub fn minimal_dark() -> Theme {
    let spec = ThemeSpec {
        name: "minimal-dark",
        fg: "#e4e4e7",
        bg: "#0e0e10",
        cursor: "#e4e4e7",
        cursor_accent: "#0e0e10",
        selection: "#3f3f46",
        selection_fg: "#ffffff",
        ansi: [
            "#27272a", "#ef4444", "#22c55e", "#eab308", "#3b82f6", "#a855f7", "#06b6d4", "#d4d4d8",
            "#52525b", "#f87171", "#4ade80", "#facc15", "#60a5fa", "#c084fc", "#22d3ee", "#fafafa",
        ],
    };
    spec.build()
}

/// Tokyo Night color scheme.
#[must_use]
pub fn tokyo_night() -> Theme {
    let spec = ThemeSpec {
        name: "tokyo-night",
        fg: "#c0caf5",
        bg: "#1a1b26",
        cursor: "#c0caf5",
        cursor_accent: "#1a1b26",
        selection: "#283457",
        selection_fg: "#c0caf5",
        ansi: [
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    };
    spec.build()
}

/// Catppuccin Mocha color scheme.
#[must_use]
pub fn catppuccin_mocha() -> Theme {
    let spec = ThemeSpec {
        name: "catppuccin-mocha",
        fg: "#cdd6f4",
        bg: "#1e1e2e",
        cursor: "#f5e0dc",
        cursor_accent: "#1e1e2e",
        selection: "#45475a",
        selection_fg: "#cdd6f4",
        ansi: [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#cba6f7", "#94e2d5", "#a6adc8",
        ],
    };
    spec.build()
}

/// Dracula color scheme.
#[must_use]
pub fn dracula() -> Theme {
    let spec = ThemeSpec {
        name: "dracula",
        fg: "#f8f8f2",
        bg: "#282a36",
        cursor: "#f8f8f2",
        cursor_accent: "#282a36",
        selection: "#44475a",
        selection_fg: "#f8f8f2",
        ansi: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    };
    spec.build()
}

/// Solarized Dark color scheme.
#[must_use]
pub fn solarized_dark() -> Theme {
    let spec = ThemeSpec {
        name: "solarized-dark",
        fg: "#839496",
        bg: "#002b36",
        cursor: "#839496",
        cursor_accent: "#002b36",
        selection: "#073642",
        selection_fg: "#839496",
        ansi: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    };
    spec.build()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compact specification for building a `Theme` from hex strings.
pub(super) struct ThemeSpec {
    pub(super) name: &'static str,
    pub(super) fg: &'static str,
    pub(super) bg: &'static str,
    pub(super) cursor: &'static str,
    pub(super) cursor_accent: &'static str,
    pub(super) selection: &'static str,
    pub(super) selection_fg: &'static str,
    pub(super) ansi: [&'static str; 16],
}

impl ThemeSpec {
    /// Build a `Theme` from a borrowed spec (used by community presets).
    pub(super) fn build_ref(&self) -> Theme {
        let foreground = hex_or_fallback(self.fg);
        let background = hex_or_fallback(self.bg);
        let cursor = hex_or_fallback(self.cursor);
        let cursor_accent = hex_or_fallback(self.cursor_accent);
        let selection = hex_or_fallback(self.selection);
        let selection_foreground = hex_or_fallback(self.selection_fg);

        let mut ansi_colors = [[0.0_f32; 4]; 16];
        for (idx, hex) in self.ansi.iter().enumerate() {
            if let Some(slot) = ansi_colors.get_mut(idx) {
                *slot = hex_or_fallback(hex);
            }
        }

        let chrome = Theme::derive_chrome(foreground, background, &ansi_colors);

        Theme {
            name: Cow::Borrowed(self.name),
            foreground,
            background,
            cursor,
            cursor_accent,
            selection,
            selection_foreground,
            ansi_colors,
            chrome,
        }
    }

    /// Convert the hex-based specification into a fully resolved `Theme`.
    fn build(self) -> Theme {
        self.build_ref()
    }
}

/// Parse a hex color, falling back to opaque black on error.
///
/// Only used for hard-coded preset strings that are known-valid,
/// so the fallback path is effectively unreachable.
fn hex_or_fallback(hex: &str) -> [f32; 4] {
    hex_to_rgba(hex).unwrap_or([0.0, 0.0, 0.0, 1.0])
}

/// Parse a two-character hex slice from `hex` at `range` into a `u8`.
fn parse_hex_channel(
    hex: &str,
    range: std::ops::Range<usize>,
    channel_name: &str,
) -> Result<u8, ScribeError> {
    let slice = hex.get(range).ok_or_else(|| ScribeError::ThemeParse {
        reason: format!("invalid {channel_name} channel slice"),
    })?;
    u8::from_str_radix(slice, 16).map_err(|err| ScribeError::ThemeParse {
        reason: format!("invalid {channel_name} channel: {err}"),
    })
}

/// Lighten an sRGB color by adding `amount` to each RGB channel, clamped to 1.0.
fn lighten(color: [f32; 4], amount: f32) -> [f32; 4] {
    let red = (color.first().copied().unwrap_or(0.0) + amount).min(1.0);
    let green = (color.get(1).copied().unwrap_or(0.0) + amount).min(1.0);
    let blue = (color.get(2).copied().unwrap_or(0.0) + amount).min(1.0);
    let alpha = color.get(3).copied().unwrap_or(1.0);
    [red, green, blue, alpha]
}

/// Return a copy of `color` with the alpha channel replaced.
fn with_alpha(color: [f32; 4], new_alpha: f32) -> [f32; 4] {
    let red = color.first().copied().unwrap_or(0.0);
    let green = color.get(1).copied().unwrap_or(0.0);
    let blue = color.get(2).copied().unwrap_or(0.0);
    [red, green, blue, new_alpha]
}

/// Clamp and convert a 0.0..=1.0 float channel to a u8.
fn channel_to_u8(value: f32) -> u8 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "value is clamped to 0..=255 so truncation to u8 is safe"
    )]
    #[allow(clippy::cast_sign_loss, reason = "value is clamped to 0..=255 so it is non-negative")]
    let byte = (value.mul_add(255.0, 0.5).clamp(0.0, 255.0)) as u8;
    byte
}
