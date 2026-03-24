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
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ScreenColor {
    Named(u16),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

/// Cell attribute flags.
#[allow(
    clippy::struct_excessive_bools,
    reason = "terminal cell attributes are inherently boolean flags; a bitfield enum would obscure semantics"
)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CellFlags {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub dim: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub wide: bool,
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
}
