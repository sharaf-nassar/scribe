//! GPU-rendered tab bar at the top of each pane.
//!
//! Generates [`CellInstance`] quads for the tab bar background and text,
//! using the same glyph atlas as the terminal grid. The instances are
//! collected into the same buffer and drawn in a single render pass.

use scribe_common::theme::ChromeColors;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use scribe_common::ids::WorkspaceId;

use crate::layout::Rect;

/// Colors for the tab bar, derived from the theme's [`ChromeColors`].
pub struct TabBarColors {
    pub bg: [f32; 4],
    pub active_bg: [f32; 4],
    pub text: [f32; 4],
    pub active_text: [f32; 4],
    #[allow(dead_code, reason = "reserved for future bottom border rendering")]
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

/// Per-tab data for rendering.
pub struct TabData {
    /// Tab title (e.g. shell name, process title).
    pub title: String,
    /// Whether this tab is the active/focused tab in its workspace.
    pub is_active: bool,
    /// AI state indicator colour. `None` when no active AI state.
    pub ai_indicator: Option<[f32; 4]>,
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
    tab_bar_height: f32,
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
            size: [cell_w, tab_bar_height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: bg,
            bg_color: bg,
        });
    }
}

/// Height of the bottom separator line in pixels.
const SEPARATOR_HEIGHT: f32 = 1.0;

/// Build a 1px separator line at the bottom of a pane's tab bar area.
///
/// Gives a clear visual boundary between the tab bar and terminal content.
pub fn build_tab_bar_separator(
    out: &mut Vec<CellInstance>,
    rect: Rect,
    cell_size: (f32, f32),
    color: [f32; 4],
    tab_bar_height: f32,
) {
    let (cell_w, _) = cell_size;
    if cell_w <= 0.0 {
        return;
    }

    let separator_y = rect.y + tab_bar_height - SEPARATOR_HEIGHT;
    let cols = columns_in_width(rect.width, cell_w);

    for col_idx in 0..cols {
        #[allow(
            clippy::cast_precision_loss,
            reason = "column index is a small positive integer fitting in f32"
        )]
        let x = rect.x + col_idx as f32 * cell_w;
        out.push(CellInstance {
            pos: [x, separator_y],
            size: [cell_w, SEPARATOR_HEIGHT],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: color,
            bg_color: color,
        });
    }
}

/// Pre-collected workspace-level data for tab bar text rendering.
///
/// Gathered at the call site (where workspace metadata is accessible) and
/// passed into `build_all_instances` to avoid borrow conflicts.
pub struct WorkspaceTabBarData {
    /// Workspace identity (used to map tab clicks to the correct workspace).
    pub ws_id: WorkspaceId,
    /// Full workspace rect (the tab bar spans its entire width).
    pub ws_rect: Rect,
    /// Tab data for each tab in the workspace.
    pub tabs: Vec<TabData>,
    /// Workspace badge: `(workspace_name, accent_color)`. `None` when single workspace.
    pub badge: Option<(String, [f32; 4])>,
    /// Whether the active tab in this workspace has multiple panes.
    pub has_multiple_panes: bool,
}

/// Parameters for building tab bar text instances.
pub struct TabBarTextParams<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    /// Tab data for each tab in the workspace.
    pub tabs: &'a [TabData],
    /// Workspace badge: `(workspace_name, accent_color)`. `None` when single workspace.
    pub badge: Option<(&'a str, [f32; 4])>,
    /// Whether to render the gear icon on the far right.
    pub show_gear: bool,
    /// Whether to render the equalize icon left of the gear.
    pub show_equalize: bool,
    pub colors: &'a TabBarColors,
    /// Closure that resolves a character to atlas UV coordinates.
    /// Returns `(uv_min, uv_max)`. `FnMut` because atlas rasterization
    /// may occur for uncached glyphs.
    pub resolve_glyph: &'a mut dyn FnMut(char) -> ([f32; 2], [f32; 2]),
    /// Tab bar height in pixels (from config).
    pub tab_bar_height: f32,
    /// AI indicator bar height in pixels (from config).
    pub indicator_height: f32,
}

/// Clickable regions produced by [`build_tab_bar_text`].
pub struct TabBarHitTargets {
    /// `(tab_index, clickable_rect)` for each rendered tab.
    pub tab_rects: Vec<(usize, Rect)>,
    /// Clickable rect for the gear icon, if rendered.
    pub gear_rect: Option<Rect>,
    /// Clickable rect for the equalize icon, if rendered.
    pub equalize_rect: Option<Rect>,
}

/// Build cell instances for the tab bar text overlay.
///
/// Returns the rendered instances and hit-test targets for click handling.
pub fn build_tab_bar_text(
    params: &mut TabBarTextParams<'_>,
) -> (Vec<CellInstance>, TabBarHitTargets) {
    let (cell_w, _cell_h) = params.cell_size;
    if cell_w <= 0.0 {
        return (
            Vec::new(),
            TabBarHitTargets { tab_rects: Vec::new(), gear_rect: None, equalize_rect: None },
        );
    }

    let max_cols = columns_in_width(params.rect.width, cell_w);
    let mut instances = Vec::new();
    let mut col: usize = 0;
    let mut hit_targets =
        TabBarHitTargets { tab_rects: Vec::new(), gear_rect: None, equalize_rect: None };

    // Reserve columns for the gear icon on the far right (2 cols: space + gear).
    let gear_cols: usize = if params.show_gear { 2 } else { 0 };
    // Reserve columns for the equalize icon left of gear (2 cols: space + icon).
    let equalize_cols: usize = if params.show_equalize { 2 } else { 0 };
    let content_cols = max_cols.saturating_sub(gear_cols).saturating_sub(equalize_cols);

    // Render workspace badge if present.
    if let Some((ws_name, accent_color)) = params.badge {
        col =
            render_badge(&mut instances, col, content_cols, cell_w, params, ws_name, accent_color);
    }

    // Render tab labels (and indicator bars for tabs with active AI state).
    col = render_tabs(&mut instances, &mut hit_targets, col, content_cols, cell_w, params);

    // Render equalize icon left of gear if requested.
    if params.show_equalize {
        col = render_equalize(&mut instances, &mut hit_targets, col, max_cols, gear_cols, params);
    }

    // Render gear icon on the far right if requested.
    if params.show_gear {
        render_gear(&mut instances, &mut hit_targets, col, max_cols, params);
    }

    (instances, hit_targets)
}

/// Render the workspace badge: colored dot + space + name + 16px gap.
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context from build_tab_bar_text"
)]
fn render_badge(
    instances: &mut Vec<CellInstance>,
    mut col: usize,
    max_cols: usize,
    cell_w: f32,
    params: &mut TabBarTextParams<'_>,
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

/// Render tab labels with hit targets for click handling, plus AI indicator
/// bars underneath tabs that have an active AI state.
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
    params: &mut TabBarTextParams<'_>,
) -> usize {
    let bg = params.colors.bg;

    for (tab_idx, tab) in params.tabs.iter().enumerate() {
        let fg = if tab.is_active { params.colors.active_text } else { params.colors.text };
        let tab_bg = if tab.is_active { params.colors.active_bg } else { bg };
        let tab_start_col = col;

        col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
        for ch in tab.title.chars() {
            col = emit_char(instances, ch, col, max_cols, params, fg, tab_bg);
        }
        col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);

        let tab_width_cols = col - tab_start_col;

        #[allow(
            clippy::cast_precision_loss,
            reason = "column indices are small positive integers fitting in f32"
        )]
        let tab_x = params.rect.x + tab_start_col as f32 * cell_w;
        #[allow(
            clippy::cast_precision_loss,
            reason = "column indices are small positive integers fitting in f32"
        )]
        let tab_width = tab_width_cols as f32 * cell_w;

        hit_targets.tab_rects.push((
            tab_idx,
            Rect { x: tab_x, y: params.rect.y, width: tab_width, height: params.tab_bar_height },
        ));

        // Render AI indicator bar above this tab if active.
        if let Some(indicator_color) = tab.ai_indicator {
            render_indicator_bar(
                instances,
                params.rect.y,
                indicator_color,
                tab_x,
                tab_width_cols,
                cell_w,
                params.indicator_height,
            );
        }
    }

    col
}

/// Render a thin coloured indicator bar above a tab (at the top of the tab bar).
#[allow(clippy::too_many_arguments, reason = "render helper needs tab geometry and colour context")]
#[allow(
    clippy::cast_precision_loss,
    reason = "column indices are small positive integers fitting in f32"
)]
fn render_indicator_bar(
    instances: &mut Vec<CellInstance>,
    rect_y: f32,
    color: [f32; 4],
    tab_x: f32,
    tab_width_cols: usize,
    cell_w: f32,
    indicator_height: f32,
) {
    // Position at the very top of the tab bar area.
    let bar_y = rect_y;

    // Solid colour bar spanning the full tab width.
    for bar_col in 0..tab_width_cols {
        let bx = tab_x + bar_col as f32 * cell_w;
        instances.push(CellInstance {
            pos: [bx, bar_y],
            size: [cell_w, indicator_height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: color,
            bg_color: color,
        });
    }
}

/// Render the gear icon on the far right of the tab bar.
fn render_gear(
    instances: &mut Vec<CellInstance>,
    hit_targets: &mut TabBarHitTargets,
    mut col: usize,
    max_cols: usize,
    params: &mut TabBarTextParams<'_>,
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
        hit_targets.gear_rect = Some(Rect {
            x: gear_x,
            y: params.rect.y,
            width: gear_width,
            height: params.tab_bar_height,
        });
    }
}

/// Render the equalize icon (⊞) just left of the gear icon's reserved space.
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context from build_tab_bar_text"
)]
fn render_equalize(
    instances: &mut Vec<CellInstance>,
    hit_targets: &mut TabBarHitTargets,
    mut col: usize,
    max_cols: usize,
    gear_cols: usize,
    params: &mut TabBarTextParams<'_>,
) -> usize {
    // Need at least 2 cols for equalize + gear reserved space.
    if max_cols < 2 {
        return col;
    }

    let bg = params.colors.bg;

    // Equalize icon sits at the rightmost column before the gear's reserved space.
    let equalize_col = max_cols.saturating_sub(gear_cols).saturating_sub(1);

    // Fill gap between last tab and equalize icon with background.
    while col < equalize_col {
        col = emit_char(instances, ' ', col, max_cols, params, params.colors.text, bg);
    }

    let equalize_start_col = col;
    col = emit_char(instances, '\u{229E}', col, max_cols, params, params.colors.text, bg);

    #[allow(
        clippy::cast_precision_loss,
        reason = "column index is a small positive integer fitting in f32"
    )]
    {
        let (cell_w, _) = params.cell_size;
        let equalize_x = params.rect.x + equalize_start_col as f32 * cell_w;
        let equalize_width = (col - equalize_start_col) as f32 * cell_w;
        hit_targets.equalize_rect = Some(Rect {
            x: equalize_x,
            y: params.rect.y,
            width: equalize_width,
            height: params.tab_bar_height,
        });
    }

    col
}

/// Emit a single character instance at the given column, returning the next
/// column index.
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
    params: &mut TabBarTextParams<'_>,
    fg: [f32; 4],
    bg: [f32; 4],
) -> usize {
    if col >= max_cols {
        return col;
    }

    let (cell_w, cell_h) = params.cell_size;
    let x = params.rect.x + col as f32 * cell_w;
    let y = params.rect.y + ((params.tab_bar_height - cell_h) / 2.0).max(0.0);
    let (uv_min, uv_max) = (params.resolve_glyph)(ch);

    instances.push(CellInstance {
        pos: [x, y],
        size: [0.0, 0.0],
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
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "gap_px / cell_w yields a small positive value fitting in usize"
)]
fn gap_columns(gap_px: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 { 0 } else { (gap_px / cell_w).ceil() as usize }
}
