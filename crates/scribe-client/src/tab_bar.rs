//! GPU-rendered tab bar at the top of each pane.
//!
//! Generates [`CellInstance`] quads for the tab bar background and text,
//! using the same glyph atlas as the terminal grid. The instances are
//! collected into the same buffer and drawn in a single render pass.

use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Height of the tab bar in pixels.
#[allow(dead_code, reason = "exported constant for pane content area calculation")]
pub const TAB_BAR_HEIGHT: f32 = 24.0;

/// Tab bar background colour (dark grey).
const TAB_BG: [f32; 4] = [0.12, 0.12, 0.14, 1.0];

/// Tab bar text colour (light grey).
#[allow(dead_code, reason = "used by build_tab_bar_text for unfocused tab text")]
const TAB_FG: [f32; 4] = [0.75, 0.75, 0.75, 1.0];

/// Focused tab bar background colour (slightly brighter).
const TAB_BG_FOCUSED: [f32; 4] = [0.18, 0.18, 0.22, 1.0];

/// Focused tab bar text colour (white).
#[allow(dead_code, reason = "used by build_tab_bar_text for focused tab text")]
const TAB_FG_FOCUSED: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// Workspace badge background colour (accent blue).
#[allow(dead_code, reason = "used by build_tab_bar_text for workspace badges")]
const BADGE_BG: [f32; 4] = [0.2, 0.4, 0.8, 1.0];

/// Workspace badge text colour (white).
#[allow(dead_code, reason = "used by build_tab_bar_text for workspace badges")]
const BADGE_FG: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// Build cell instances for a pane's tab bar background.
///
/// Pushes a row of solid-colour quads (no glyph, `uv_min == uv_max == [0,0]`)
/// that fill the tab bar area into `out`. `cell_size` is `(width, height)` from
/// the font. Pushing directly into the caller's `Vec` avoids a per-call heap
/// allocation.
pub fn build_tab_bar_bg(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    cell_size: (f32, f32),
    focused: bool,
) {
    let (cell_w, _cell_h) = cell_size;
    if cell_w <= 0.0 {
        return;
    }

    let bg = if focused { TAB_BG_FOCUSED } else { TAB_BG };

    // Fill the tab bar area with background quads, one per cell-width column.
    let cols = columns_in_width(rect.width, cell_w);

    for col_idx in 0..cols {
        #[allow(
            clippy::cast_precision_loss,
            reason = "column index is a small positive integer fitting in f32"
        )]
        let x = rect.x + col_idx as f32 * cell_w;
        out.push(CellInstance {
            pos: [x, rect.y],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
        });
    }
}

/// Parameters for building tab bar text instances.
#[allow(dead_code, reason = "public API for tab bar text rendering, used in later phases")]
pub struct TabBarTextParams<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    pub focused: bool,
    pub workspace_name: Option<&'a str>,
    pub title: &'a str,
    /// Closure that resolves a character to atlas UV coordinates.
    /// Returns `(uv_min, uv_max)`.
    pub resolve_glyph: &'a dyn Fn(char) -> ([f32; 2], [f32; 2]),
}

/// Build cell instances for the tab bar text overlay.
///
/// Renders `[workspace_name] title` using the glyph atlas.
#[allow(dead_code, reason = "public API for tab bar text rendering, used in later phases")]
pub fn build_tab_bar_text(params: &TabBarTextParams<'_>) -> Vec<CellInstance> {
    let (cell_w, _cell_h) = params.cell_size;
    if cell_w <= 0.0 {
        return Vec::new();
    }

    let max_cols = columns_in_width(params.rect.width, cell_w);
    let mut instances = Vec::new();
    let mut col: usize = 0;

    let background = if params.focused { TAB_BG_FOCUSED } else { TAB_BG };
    let foreground = if params.focused { TAB_FG_FOCUSED } else { TAB_FG };

    // Render workspace badge if present: " workspace_name "
    if let Some(ws_name) = params.workspace_name {
        // Leading space.
        col = emit_char(&mut instances, ' ', col, max_cols, params, BADGE_FG, BADGE_BG);

        for ch in ws_name.chars() {
            col = emit_char(&mut instances, ch, col, max_cols, params, BADGE_FG, BADGE_BG);
        }

        // Trailing space after badge.
        col = emit_char(&mut instances, ' ', col, max_cols, params, BADGE_FG, BADGE_BG);

        // Separator space.
        col = emit_char(&mut instances, ' ', col, max_cols, params, foreground, background);
    }

    // Render the title.
    for ch in params.title.chars() {
        col = emit_char(&mut instances, ch, col, max_cols, params, foreground, background);
    }

    instances
}

/// Emit a single character instance at the given column, returning the next
/// column index.
#[allow(dead_code, reason = "called by build_tab_bar_text")]
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context parameters"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "column index is a small positive integer fitting in f32"
)]
fn emit_char(
    instances: &mut Vec<CellInstance>,
    ch: char,
    col: usize,
    max_cols: usize,
    params: &TabBarTextParams<'_>,
    fg: [f32; 4],
    bg: [f32; 4],
) -> usize {
    if col >= max_cols {
        return col;
    }

    let (cell_w, _cell_h) = params.cell_size;
    let x = params.rect.x + col as f32 * cell_w;
    let (uv_min, uv_max) = (params.resolve_glyph)(ch);

    instances.push(CellInstance {
        pos: [x, params.rect.y],
        uv_min,
        uv_max,
        fg_color: fg,
        bg_color: bg,
    });

    col + 1
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
