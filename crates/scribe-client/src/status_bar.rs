//! GPU-rendered status bar at the bottom of each pane.
//!
//! Generates [`CellInstance`] quads for the status bar background and text,
//! using the same glyph atlas as the terminal grid. The instances are
//! collected into the same buffer and drawn in a single render pass.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Height of the status bar in pixels.
pub const STATUS_BAR_HEIGHT: f32 = 24.0;

/// Data needed to render the status bar.
pub struct StatusBarData<'a> {
    pub connected: bool,
    pub shell_name: &'a str,
    pub pane_count: usize,
    pub git_branch: Option<&'a str>,
    pub grid_size: (u16, u16),
}

/// Fallback green when ANSI index 2 is unavailable.
const FALLBACK_GREEN: [f32; 4] = [0.4, 0.9, 0.5, 1.0];
/// Fallback red when ANSI index 1 is unavailable.
const FALLBACK_RED: [f32; 4] = [1.0, 0.2, 0.2, 1.0];

/// Colors for the status bar, derived from the theme's [`ChromeColors`]
/// and ANSI palette.
pub struct StatusBarColors {
    pub bg: [f32; 4],
    pub text: [f32; 4],
    pub accent: [f32; 4],
    pub separator: [f32; 4],
    /// Color for the connection-status dot when connected (ANSI green).
    pub connected_dot: [f32; 4],
    /// Color for the connection-status dot when disconnected (ANSI red).
    pub disconnected_dot: [f32; 4],
}

impl StatusBarColors {
    /// Build status bar colors from chrome colors and the theme's ANSI palette.
    pub fn from_theme(chrome: &ChromeColors, ansi_colors: &[[f32; 4]; 16]) -> Self {
        Self {
            bg: srgb_to_linear_rgba(chrome.status_bar_bg),
            text: srgb_to_linear_rgba(chrome.status_bar_text),
            accent: srgb_to_linear_rgba(chrome.accent),
            separator: srgb_to_linear_rgba(chrome.divider),
            connected_dot: srgb_to_linear_rgba(
                ansi_colors.get(2).copied().unwrap_or(FALLBACK_GREEN),
            ),
            disconnected_dot: srgb_to_linear_rgba(
                ansi_colors.get(1).copied().unwrap_or(FALLBACK_RED),
            ),
        }
    }
}

/// Build cell instances for the status bar at the bottom of a workspace rect.
///
/// Pushes background quads and text glyph instances into `out`.
/// `resolve_glyph` maps a character to atlas UV coordinates `(uv_min, uv_max)`.
#[allow(
    clippy::too_many_arguments,
    reason = "needs rect, cell size, colors, data, and glyph resolver for full status bar rendering"
)]
pub fn build_status_bar(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    cell_size: (f32, f32),
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let (cell_w, _cell_h) = cell_size;
    if cell_w <= 0.0 {
        return;
    }

    let bar_y = rect.y + rect.height - STATUS_BAR_HEIGHT;
    let max_cols = columns_in_width(rect.width, cell_w);
    let mut w = BarWriter { out, x_origin: rect.x, y: bar_y, cell_w, max_cols, col: 0 };

    // Background fill.
    build_background(w.out, w.x_origin, w.y, w.max_cols, w.cell_w, colors.bg);

    // Left side: connection indicator + shell name.
    let col = render_left_side(&mut w, colors, data, resolve_glyph);
    w.col = col;

    // Right side: pane count, git branch, grid size.
    render_right_side(&mut w, colors, data, resolve_glyph);
}

/// Mutable writer state for emitting status bar characters.
struct BarWriter<'a> {
    out: &'a mut Vec<CellInstance>,
    x_origin: f32,
    y: f32,
    cell_w: f32,
    max_cols: usize,
    col: usize,
}

impl BarWriter<'_> {
    /// Emit a single character at the current column with the given colors.
    #[allow(
        clippy::cast_precision_loss,
        reason = "column index is a small positive integer fitting in f32"
    )]
    fn put(
        &mut self,
        ch: char,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        if self.col >= self.max_cols {
            return;
        }
        let x = self.x_origin + self.col as f32 * self.cell_w;
        let (uv_min, uv_max) = resolve_glyph(ch);
        self.out.push(CellInstance {
            pos: [x, self.y],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
        });
        self.col += 1;
    }

    /// Emit a string with the given colors.
    fn put_str(
        &mut self,
        s: &str,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        for ch in s.chars() {
            self.put(ch, fg, bg, resolve_glyph);
        }
    }

    /// Fill from current column to target with spaces.
    fn pad_to(
        &mut self,
        target: usize,
        fg: [f32; 4],
        bg: [f32; 4],
        resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    ) {
        while self.col < target {
            self.put(' ', fg, bg, resolve_glyph);
        }
    }
}

/// Render the left side: space, connection dot, space, shell name.
fn render_left_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) -> usize {
    w.put(' ', colors.text, colors.bg, resolve_glyph);

    let dot_color = if data.connected { colors.connected_dot } else { colors.disconnected_dot };
    w.put('\u{25CF}', dot_color, colors.bg, resolve_glyph);
    w.put(' ', colors.text, colors.bg, resolve_glyph);
    w.put_str(data.shell_name, colors.text, colors.bg, resolve_glyph);

    w.col
}

/// Render the right side: pane count | git branch | grid size.
fn render_right_side(
    w: &mut BarWriter<'_>,
    colors: &StatusBarColors,
    data: &StatusBarData<'_>,
    resolve_glyph: &mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
) {
    let segments = build_right_segments(data, colors);
    let right_cols: usize = segments.iter().map(|s| s.text.len()).sum();
    let right_start = w.max_cols.saturating_sub(right_cols + 1);

    w.pad_to(right_start, colors.text, colors.bg, resolve_glyph);

    for seg in &segments {
        w.put_str(&seg.text, seg.color, colors.bg, resolve_glyph);
    }

    w.pad_to(w.max_cols, colors.text, colors.bg, resolve_glyph);
}

/// A styled text segment for the right side of the status bar.
struct RightSegment {
    text: String,
    color: [f32; 4],
}

/// Build the right-side text segments: pane count, git branch, grid size.
fn build_right_segments(data: &StatusBarData<'_>, colors: &StatusBarColors) -> Vec<RightSegment> {
    let mut segs = Vec::new();

    if data.pane_count > 1 {
        segs.push(RightSegment { text: format!("{} panes", data.pane_count), color: colors.text });
    }

    if let Some(branch) = data.git_branch {
        if !segs.is_empty() {
            segs.push(RightSegment { text: String::from(" | "), color: colors.separator });
        }
        segs.push(RightSegment { text: String::from(branch), color: colors.accent });
    }

    if !segs.is_empty() {
        segs.push(RightSegment { text: String::from(" | "), color: colors.separator });
    }
    segs.push(RightSegment {
        text: format!("{}x{}", data.grid_size.0, data.grid_size.1),
        color: colors.text,
    });
    segs.push(RightSegment { text: String::from(" "), color: colors.text });

    segs
}

/// Fill columns with background quads (no glyph).
#[allow(
    clippy::too_many_arguments,
    reason = "helper function needs position, column count, cell width, and color"
)]
fn build_background(
    out: &mut Vec<CellInstance>,
    x_origin: f32,
    y: f32,
    cols: usize,
    cell_w: f32,
    bg: [f32; 4],
) {
    for col_idx in 0..cols {
        #[allow(
            clippy::cast_precision_loss,
            reason = "column index is a small positive integer fitting in f32"
        )]
        let x = x_origin + col_idx as f32 * cell_w;
        out.push(CellInstance {
            pos: [x, y],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
        });
    }
}

/// Calculate how many cell-width columns fit in a given pixel width.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "width / cell_w yields a small positive value fitting in usize"
)]
fn columns_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 { 0 } else { (width / cell_w) as usize }
}
