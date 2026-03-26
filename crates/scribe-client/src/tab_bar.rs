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

/// Compute the tab bar height in pixels for a workspace with the given parameters.
///
/// Accounts for multi-row stacking when there are more tabs than fit in one row.
#[allow(
    clippy::cast_precision_loss,
    reason = "tab and column counts are small positive integers fitting in f32"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "all parameters are needed to compute multi-row tab bar height"
)]
pub fn compute_tab_bar_height(
    tab_count: usize,
    ws_width: f32,
    tab_width_chars: u16,
    cell_w: f32,
    row_height: f32,
    badge_cols: usize,
) -> f32 {
    if cell_w <= 0.0 || row_height <= 0.0 {
        return row_height.max(1.0);
    }
    let gear_cols: usize = 2;
    let total_cols = columns_in_width(ws_width, cell_w);
    let available = total_cols.saturating_sub(badge_cols).saturating_sub(gear_cols);
    let tab_w = usize::from(tab_width_chars).max(1);
    let tabs_per_row = (available / tab_w).max(1);
    let effective_count = tab_count.max(1);
    let rows = effective_count.div_ceil(tabs_per_row);
    rows as f32 * row_height
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
    /// Pre-computed tab bar height for this workspace (accounts for multi-row stacking).
    pub tab_bar_height: f32,
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
    /// Fixed tab width in characters (includes leading/trailing padding).
    pub tab_width: u16,
    /// Which tab's close button is shown (hovered). `None` = no hover.
    pub hovered_tab_close: Option<usize>,
    /// Per-tab pixel X offsets for slide animation. Empty slice when no drag active.
    pub tab_offsets: &'a [f32],
    /// Index of the tab being dragged, if any.
    pub dragging_tab: Option<usize>,
    /// Current cursor X position during drag.
    pub drag_cursor_x: f32,
    /// Grab offset (cursor X minus tab left edge at drag start).
    pub drag_grab_offset: f32,
    /// Accent color for the drag underline.
    pub accent_color: [f32; 4],
}

/// Clickable regions produced by [`build_tab_bar_text`].
pub struct TabBarHitTargets {
    /// `(tab_index, clickable_rect)` for each rendered tab.
    pub tab_rects: Vec<(usize, Rect)>,
    /// Clickable rect for the gear icon, if rendered.
    pub gear_rect: Option<Rect>,
    /// Clickable rect for the equalize icon, if rendered.
    pub equalize_rect: Option<Rect>,
    /// Close button clickable regions per tab: `(tab_index, rect)`.
    pub close_rects: Vec<(usize, Rect)>,
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
            TabBarHitTargets {
                tab_rects: Vec::new(),
                gear_rect: None,
                equalize_rect: None,
                close_rects: Vec::new(),
            },
        );
    }

    let max_cols = columns_in_width(params.rect.width, cell_w);
    let mut instances = Vec::new();
    let mut col: usize = 0;
    let mut hit_targets = TabBarHitTargets {
        tab_rects: Vec::new(),
        gear_rect: None,
        equalize_rect: None,
        close_rects: Vec::new(),
    };

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
///
/// Tabs have a fixed width (`params.tab_width` columns) and wrap to new rows
/// when they would exceed `max_cols`. Returns the column where content ends
/// on row 0 (for gear/equalize icon positioning).
#[allow(
    clippy::too_many_arguments,
    reason = "helper function that needs all render context from build_tab_bar_text"
)]
#[allow(
    clippy::cast_precision_loss,
    reason = "column and row indices are small positive integers fitting in f32"
)]
#[allow(
    clippy::too_many_lines,
    reason = "multi-row fixed-width tab rendering with hit targets, AI indicators, and drag offsets"
)]
#[allow(
    clippy::cognitive_complexity,
    reason = "multi-row fixed-width tab rendering with hit targets, AI indicators, and drag offsets"
)]
fn render_tabs(
    instances: &mut Vec<CellInstance>,
    hit_targets: &mut TabBarHitTargets,
    start_col: usize,
    max_cols: usize,
    cell_w: f32,
    params: &mut TabBarTextParams<'_>,
) -> usize {
    let bg = params.colors.bg;
    let tab_w = usize::from(params.tab_width).max(1);

    // Compute the single-row height from total height / number of rows.
    let tab_count = params.tabs.len().max(1);
    let available = max_cols.saturating_sub(start_col);
    let tabs_per_row = (available / tab_w).max(1);
    let num_rows = tab_count.div_ceil(tabs_per_row);
    let row_height =
        if num_rows > 0 { params.tab_bar_height / num_rows as f32 } else { params.tab_bar_height };

    // Save original params that we temporarily mutate per row.
    let base_y = params.rect.y;
    let total_tab_bar_h = params.tab_bar_height;

    let mut row: usize = 0;
    let mut col: usize = start_col;
    // Track where row 0 ends (for gear/equalize positioning).
    let mut row0_end_col: usize = start_col;

    for (tab_idx, tab) in params.tabs.iter().enumerate() {
        // Wrap to next row if this tab would not fit.
        if tab_idx > 0 && col + tab_w > max_cols {
            row += 1;
            col = start_col;
        }

        let fg = if tab.is_active { params.colors.active_text } else { params.colors.text };
        let tab_bg = if tab.is_active { params.colors.active_bg } else { bg };
        let tab_start_col = col;
        let row_base_y = base_y + row as f32 * row_height;

        // Set per-row context so emit_char positions glyphs correctly.
        params.rect.y = row_base_y;
        params.tab_bar_height = row_height;

        // Determine title display:
        // Each tab has `tab_w` columns: 1 left-pad + title + 1 right-pad.
        // If close button shown, the last 2 chars are " ×" instead of title overflow.
        let show_close = params.hovered_tab_close == Some(tab_idx);
        let available_title = if tab_w >= 4 {
            if show_close && tab_w >= 6 {
                tab_w.saturating_sub(4) // 1 left-pad + title + " ×"
            } else {
                tab_w.saturating_sub(2) // 1 left-pad + title + 1 right-pad
            }
        } else {
            tab_w.saturating_sub(2)
        };

        // Build the display title: truncate with '…' if too long, or pad with spaces.
        let title_chars: Vec<char> = tab.title.chars().collect();
        let display_title: Vec<char> = if tab_w >= 4 && title_chars.len() > available_title {
            let keep = available_title.saturating_sub(1);
            let mut t: Vec<char> = title_chars.get(..keep).map_or_else(Vec::new, <[char]>::to_vec);
            t.push('\u{2026}'); // '…'
            t
        } else {
            let mut t = title_chars;
            while t.len() < available_title {
                t.push(' ');
            }
            t
        };

        // Record the instance index before emitting this tab's quads so we can
        // retroactively apply the slide offset to all of them.
        let tab_start_instance = instances.len();

        // Emit: leading space + title + (close " ×" or trailing space).
        col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
        for &ch in &display_title {
            col = emit_char(instances, ch, col, max_cols, params, fg, tab_bg);
        }
        if show_close {
            col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
            col = emit_char(instances, '\u{00D7}', col, max_cols, params, fg, tab_bg);
        } else {
            col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
        }

        // Fill any remaining columns in this fixed-width tab slot.
        let expected_end = tab_start_col + tab_w;
        while col < expected_end.min(max_cols) {
            col = emit_char(instances, ' ', col, max_cols, params, fg, tab_bg);
        }
        col = expected_end.min(max_cols);

        if row == 0 {
            row0_end_col = col;
        }

        let tab_width_px = tab_w as f32 * cell_w;
        let tab_x = params.rect.x + tab_start_col as f32 * cell_w;

        // Close-button rect: rightmost 2 columns of the tab.
        let close_x = params.rect.x + (tab_start_col + tab_w).saturating_sub(2) as f32 * cell_w;
        hit_targets.close_rects.push((
            tab_idx,
            Rect { x: close_x, y: row_base_y, width: 2.0 * cell_w, height: row_height },
        ));

        // Hit targets use logical (un-offset) positions for reliable reorder detection.
        hit_targets.tab_rects.push((
            tab_idx,
            Rect { x: tab_x, y: row_base_y, width: tab_width_px, height: row_height },
        ));

        // Render AI indicator bar at the top of this tab's row.
        if let Some(indicator_color) = tab.ai_indicator {
            render_indicator_bar(
                instances,
                row_base_y,
                indicator_color,
                tab_x,
                tab_w,
                cell_w,
                params.indicator_height,
            );
        }

        // Compute slide/drag offset for this tab.
        // tab_x is the logical (un-offset) left edge of this tab.
        let tab_offset = if params.dragging_tab == Some(tab_idx) {
            // Dragged tab follows cursor: shift so its left edge tracks the cursor.
            params.drag_cursor_x - params.drag_grab_offset - tab_x
        } else {
            params.tab_offsets.get(tab_idx).copied().unwrap_or(0.0)
        };

        // Apply offset to all instances emitted for this tab.
        if tab_offset != 0.0 {
            apply_x_offset(instances, tab_start_instance, tab_offset);
        }

        // Render accent underline at bottom of dragged tab.
        if params.dragging_tab == Some(tab_idx) {
            let underline_height = 2.0;
            let underline_y = row_base_y + row_height - underline_height;
            let visual_x = tab_x + tab_offset;
            for bar_col in 0..tab_w {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "column index is a small positive integer fitting in f32"
                )]
                let bx = visual_x + bar_col as f32 * cell_w;
                instances.push(CellInstance {
                    pos: [bx, underline_y],
                    size: [cell_w, underline_height],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg_color: params.accent_color,
                    bg_color: params.accent_color,
                });
            }
        }
    }

    // Restore original params.
    params.rect.y = base_y;
    params.tab_bar_height = total_tab_bar_h;

    // Return row 0 end column so gear/equalize render correctly in row 0.
    row0_end_col
}

/// Shift all instances from `start_idx` onward by `dx` pixels along the X axis.
fn apply_x_offset(instances: &mut [CellInstance], start_idx: usize, dx: f32) {
    let end = instances.len();
    for idx in start_idx..end {
        if let Some(inst) = instances.get_mut(idx) {
            inst.pos[0] += dx;
        }
    }
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
