//! GPU-rendered tab bar at the top of each pane.
//!
//! Generates [`CellInstance`] quads for the tab bar background and text,
//! using the same glyph atlas as the terminal grid. The instances are
//! collected into the same buffer and drawn in a single render pass.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use crate::layout::Rect;

/// Height of the tab bar in pixels.
#[allow(dead_code, reason = "exported constant for pane content area calculation")]
pub const TAB_BAR_HEIGHT: f32 = 32.0;

/// Colors for the tab bar, derived from the theme's [`ChromeColors`].
pub struct TabBarColors {
    pub bg: [f32; 4],
    #[allow(dead_code, reason = "used by build_tab_bar_text for active tab background")]
    pub active_bg: [f32; 4],
    #[allow(dead_code, reason = "used by build_tab_bar_text for inactive tab text")]
    pub text: [f32; 4],
    #[allow(dead_code, reason = "used by build_tab_bar_text for active tab text")]
    pub active_text: [f32; 4],
    #[allow(dead_code, reason = "used by future bottom border rendering")]
    pub border: [f32; 4],
}

impl From<&ChromeColors> for TabBarColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            bg: srgb_to_linear_rgba(chrome.tab_bar_bg),
            active_bg: srgb_to_linear_rgba(chrome.tab_bar_active_bg),
            text: srgb_to_linear_rgba(chrome.tab_text),
            active_text: srgb_to_linear_rgba(chrome.tab_text_active),
            border: srgb_to_linear_rgba(chrome.divider),
        }
    }
}

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
    colors: &TabBarColors,
) {
    let (cell_w, _cell_h) = cell_size;
    if cell_w <= 0.0 {
        return;
    }

    let bg = colors.bg;

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
#[allow(dead_code, reason = "public API for tab bar text rendering, used by workspace layout")]
pub struct TabBarTextParams<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    /// Tab labels as `(title, is_active)` pairs.
    pub tabs: &'a [(String, bool)],
    /// Workspace badge: `(workspace_name, accent_color)`. `None` when single workspace.
    pub badge: Option<(&'a str, [f32; 4])>,
    /// Whether to render the gear icon on the far right.
    pub show_gear: bool,
    pub colors: &'a TabBarColors,
    /// Closure that resolves a character to atlas UV coordinates.
    /// Returns `(uv_min, uv_max)`.
    pub resolve_glyph: &'a dyn Fn(char) -> ([f32; 2], [f32; 2]),
}

/// Clickable regions produced by [`build_tab_bar_text`].
#[allow(dead_code, reason = "public API for tab bar click handling, used by workspace layout")]
pub struct TabBarHitTargets {
    /// `(tab_index, clickable_rect)` for each rendered tab.
    pub tab_rects: Vec<(usize, Rect)>,
    /// Clickable rect for the gear icon, if rendered.
    pub gear_rect: Option<Rect>,
}

/// Build cell instances for the tab bar text overlay.
///
/// Returns the rendered instances and hit-test targets for click handling.
#[allow(dead_code, reason = "public API for tab bar text rendering, used by workspace layout")]
pub fn build_tab_bar_text(params: &TabBarTextParams<'_>) -> (Vec<CellInstance>, TabBarHitTargets) {
    let (cell_w, _cell_h) = params.cell_size;
    if cell_w <= 0.0 {
        return (Vec::new(), TabBarHitTargets { tab_rects: Vec::new(), gear_rect: None });
    }

    let max_cols = columns_in_width(params.rect.width, cell_w);
    let mut instances = Vec::new();
    let mut col: usize = 0;
    let mut hit_targets = TabBarHitTargets { tab_rects: Vec::new(), gear_rect: None };

    // Reserve columns for the gear icon on the far right (2 cols: space + gear).
    let gear_cols: usize = if params.show_gear { 2 } else { 0 };
    let content_cols = max_cols.saturating_sub(gear_cols);

    // Render workspace badge if present.
    if let Some((ws_name, accent_color)) = params.badge {
        col =
            render_badge(&mut instances, col, content_cols, cell_w, params, ws_name, accent_color);
    }

    // Render tab labels.
    col = render_tabs(&mut instances, &mut hit_targets, col, content_cols, cell_w, params);

    // Render gear icon on the far right if requested.
    if params.show_gear {
        render_gear(&mut instances, &mut hit_targets, col, max_cols, params);
    }

    (instances, hit_targets)
}

/// Render the workspace badge: colored dot + space + name + 16px gap.
#[allow(dead_code, reason = "called by build_tab_bar_text")]
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context from build_tab_bar_text"
)]
fn render_badge(
    instances: &mut Vec<CellInstance>,
    mut col: usize,
    max_cols: usize,
    cell_w: f32,
    params: &TabBarTextParams<'_>,
    ws_name: &str,
    accent_color: [f32; 4],
) -> usize {
    let bg = params.colors.bg;

    // Colored dot character as badge indicator.
    col = emit_char(instances, '\u{25CF}', col, max_cols, params, accent_color, bg);

    // Space after dot.
    col = emit_char(instances, ' ', col, max_cols, params, params.colors.text, bg);

    // Workspace name in normal text color.
    for ch in ws_name.chars() {
        col = emit_char(instances, ch, col, max_cols, params, params.colors.text, bg);
    }

    // 16px gap after badge (approximately 2 cell widths).
    let gap = gap_columns(16.0, cell_w);
    for _ in 0..gap {
        col = emit_char(instances, ' ', col, max_cols, params, params.colors.text, bg);
    }

    col
}

/// Render tab labels with hit targets for click handling.
#[allow(dead_code, reason = "called by build_tab_bar_text")]
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context from build_tab_bar_text"
)]
fn render_tabs(
    instances: &mut Vec<CellInstance>,
    hit_targets: &mut TabBarHitTargets,
    mut col: usize,
    max_cols: usize,
    cell_w: f32,
    params: &TabBarTextParams<'_>,
) -> usize {
    let bg = params.colors.bg;

    for (tab_idx, (title, is_active)) in params.tabs.iter().enumerate() {
        let fg = if *is_active { params.colors.active_text } else { params.colors.text };
        let tab_bg = if *is_active { params.colors.active_bg } else { bg };
        let tab_start_col = col;

        col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
        for ch in title.chars() {
            col = emit_char(instances, ch, col, max_cols, params, fg, tab_bg);
        }
        col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);

        #[allow(
            clippy::cast_precision_loss,
            reason = "column indices are small positive integers fitting in f32"
        )]
        {
            let tab_x = params.rect.x + tab_start_col as f32 * cell_w;
            let tab_width = (col - tab_start_col) as f32 * cell_w;
            hit_targets.tab_rects.push((
                tab_idx,
                Rect { x: tab_x, y: params.rect.y, width: tab_width, height: TAB_BAR_HEIGHT },
            ));
        }
    }

    col
}

/// Render the gear icon on the far right of the tab bar.
#[allow(dead_code, reason = "called by build_tab_bar_text")]
fn render_gear(
    instances: &mut Vec<CellInstance>,
    hit_targets: &mut TabBarHitTargets,
    mut col: usize,
    max_cols: usize,
    params: &TabBarTextParams<'_>,
) {
    if max_cols < 2 {
        return;
    }

    let bg = params.colors.bg;

    // Position the gear at the rightmost column.
    let gear_col = max_cols - 1;

    // Fill gap between last tab and gear with background.
    while col < gear_col {
        col = emit_char(instances, ' ', col, max_cols, params, params.colors.text, bg);
    }

    let gear_start_col = col;
    col = emit_char(instances, '\u{2699}', col, max_cols, params, params.colors.text, bg);

    #[allow(
        clippy::cast_precision_loss,
        reason = "column index is a small positive integer fitting in f32"
    )]
    {
        let (cell_w, _) = params.cell_size;
        let gear_x = params.rect.x + gear_start_col as f32 * cell_w;
        let gear_width = (col - gear_start_col) as f32 * cell_w;
        hit_targets.gear_rect =
            Some(Rect { x: gear_x, y: params.rect.y, width: gear_width, height: TAB_BAR_HEIGHT });
    }
}

/// Emit a single character instance at the given column, returning the next
/// column index.
#[allow(dead_code, reason = "called by build_tab_bar_text helpers")]
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

/// Calculate how many columns a pixel gap requires.
#[allow(dead_code, reason = "called by render_badge for badge gap spacing")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "gap_px / cell_w yields a small positive value fitting in usize"
)]
fn gap_columns(gap_px: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 { 0 } else { (gap_px / cell_w).ceil() as usize }
}
