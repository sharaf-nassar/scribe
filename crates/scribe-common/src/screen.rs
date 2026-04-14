use serde::{Deserialize, Serialize};

/// A single terminal cell, serializable for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenCell {
    pub c: char,
    pub fg: ScreenColor,
    pub bg: ScreenColor,
    pub flags: CellFlags,
}

/// Terminal color representation.
///
/// `Named` uses `u16` because `alacritty_terminal::NamedColor` has variants
/// above 255 (e.g. `Foreground = 256`, `Background = 257`, `Cursor = 258`,
/// `DimBlack = 259`, …). A `u8` would silently truncate these to 0–15.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ScreenColor {
    Named(u16),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

/// Emphasis-related cell attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellWeightFlags {
    pub bold: bool,
    pub dim: bool,
}

/// Emphasis-related cell attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellEmphasisFlags {
    #[serde(default, flatten)]
    pub weight: CellWeightFlags,
    pub italic: bool,
}

/// Decoration-related cell attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellDecorationFlags {
    pub underline: bool,
    pub strikethrough: bool,
}

/// Visibility and colour-presentation attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPresentationFlags {
    pub inverse: bool,
    pub hidden: bool,
}

/// Layout-affecting cell attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellLayoutFlags {
    pub wide: bool,
    /// Whether this cell is the last cell of a row that soft-wraps into the
    /// next row (`WRAPLINE` in alacritty).
    #[serde(default)]
    pub wrap: bool,
}

/// Cell attribute flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellFlags {
    #[serde(default, flatten)]
    pub emphasis: CellEmphasisFlags,
    #[serde(default, flatten)]
    pub decoration: CellDecorationFlags,
    #[serde(default, flatten)]
    pub presentation: CellPresentationFlags,
    #[serde(default, flatten)]
    pub layout: CellLayoutFlags,
}

impl CellFlags {
    #[must_use]
    pub const fn bold(self) -> bool {
        self.emphasis.weight.bold
    }

    pub fn set_bold(&mut self, value: bool) {
        self.emphasis.weight.bold = value;
    }

    #[must_use]
    pub const fn italic(self) -> bool {
        self.emphasis.italic
    }

    pub fn set_italic(&mut self, value: bool) {
        self.emphasis.italic = value;
    }

    #[must_use]
    pub const fn underline(self) -> bool {
        self.decoration.underline
    }

    pub fn set_underline(&mut self, value: bool) {
        self.decoration.underline = value;
    }

    #[must_use]
    pub const fn strikethrough(self) -> bool {
        self.decoration.strikethrough
    }

    pub fn set_strikethrough(&mut self, value: bool) {
        self.decoration.strikethrough = value;
    }

    #[must_use]
    pub const fn dim(self) -> bool {
        self.emphasis.weight.dim
    }

    pub fn set_dim(&mut self, value: bool) {
        self.emphasis.weight.dim = value;
    }

    #[must_use]
    pub const fn inverse(self) -> bool {
        self.presentation.inverse
    }

    pub fn set_inverse(&mut self, value: bool) {
        self.presentation.inverse = value;
    }

    #[must_use]
    pub const fn hidden(self) -> bool {
        self.presentation.hidden
    }

    pub fn set_hidden(&mut self, value: bool) {
        self.presentation.hidden = value;
    }

    #[must_use]
    pub const fn wide(self) -> bool {
        self.layout.wide
    }

    pub fn set_wide(&mut self, value: bool) {
        self.layout.wide = value;
    }

    #[must_use]
    pub const fn wrap(self) -> bool {
        self.layout.wrap
    }

    pub fn set_wrap(&mut self, value: bool) {
        self.layout.wrap = value;
    }
}

/// Cursor style for rendering.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CursorStyle {
    Block,
    Beam,
    Underline,
    HollowBlock,
}

/// A complete screen snapshot for IPC transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub cells: Vec<ScreenCell>,
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    /// Whether the terminal was in alternate screen mode when the snapshot
    /// was taken.  The client must re-enter alt screen before feeding the
    /// ANSI so that subsequent PTY output lands in the correct buffer.
    #[serde(default)]
    pub alt_screen: bool,
    /// Scrollback lines preceding the visible grid, ordered oldest-first.
    /// Each row contains `cols` cells, same as the visible grid.
    /// On reconnect the client feeds these before the visible content so
    /// they flow into the client-side scrollback buffer naturally.
    #[serde(default)]
    pub scrollback: Vec<ScreenCell>,
    /// Number of scrollback rows stored in `scrollback`.
    #[serde(default)]
    pub scrollback_rows: u32,
}
