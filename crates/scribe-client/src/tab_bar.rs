//! GPU-rendered tab bar at the top of each pane.
//!
//! Generates [`CellInstance`] quads for the tab bar background and text,
//! using the same glyph atlas as the terminal grid. The instances are
//! collected into the same buffer and drawn in a single render pass.

use scribe_common::theme::ChromeColors;
use scribe_renderer::chrome::solid_quad;
use scribe_renderer::srgb_to_linear_rgba;
use scribe_renderer::types::CellInstance;

use scribe_common::ids::WorkspaceId;

use crate::layout::Rect;
use crate::tooltip::TooltipAnchor;

type GlyphResolver<'a> = dyn FnMut(char) -> ([f32; 2], [f32; 2]) + 'a;

/// Colors for the tab bar, derived from the theme's [`ChromeColors`].
pub struct TabBarColors {
    pub bg: [f32; 4],
    pub active_bg: [f32; 4],
    pub text: [f32; 4],
    pub active_text: [f32; 4],
    pub separator: [f32; 4],
    /// Slightly lighter background for the top half of the gradient tab bar.
    pub gradient_top: [f32; 4],
}

impl From<&ChromeColors> for TabBarColors {
    fn from(chrome: &ChromeColors) -> Self {
        Self {
            bg: srgb_to_linear_rgba(chrome.tab_bar_bg),
            active_bg: srgb_to_linear_rgba(chrome.tab_bar_active_bg),
            text: srgb_to_linear_rgba(chrome.tab_text),
            active_text: srgb_to_linear_rgba(chrome.tab_text_active),
            separator: srgb_to_linear_rgba(chrome.tab_separator),
            gradient_top: srgb_to_linear_rgba(chrome.tab_bar_gradient_top),
        }
    }
}

/// Per-tab data for rendering.
#[derive(Clone)]
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
pub struct TabBarBackgroundContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub rect: Rect,
    pub colors: &'a TabBarColors,
    pub tab_bar_height: f32,
    pub active_range: Option<(f32, f32)>,
}

pub fn build_tab_bar_bg(ctx: TabBarBackgroundContext<'_>) {
    let TabBarBackgroundContext { out, rect, colors, tab_bar_height, active_range } = ctx;
    let half_h = tab_bar_height / 2.0;

    // Emit wide-span quads instead of per-column quads.
    // Split into up to three horizontal regions: before-active, active, after-active.
    // Each region gets two half-height quads for a subtle vertical gradient
    // (except active tabs which use a uniform color to stand out).
    let regions = build_bg_regions(rect, active_range);
    for (rx, rw, is_active) in regions {
        if rw <= 0.0 {
            continue;
        }
        if is_active {
            // Active tab: uniform color, no gradient.
            out.push(CellInstance {
                pos: [rx, rect.y],
                size: [rw, tab_bar_height],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: colors.active_bg,
                bg_color: colors.active_bg,
                corner_radius: 0.0,
            });
        } else {
            // Inactive region: lighter top half, normal bottom half.
            out.push(CellInstance {
                pos: [rx, rect.y],
                size: [rw, half_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: colors.gradient_top,
                bg_color: colors.gradient_top,
                corner_radius: 0.0,
            });
            out.push(CellInstance {
                pos: [rx, rect.y + half_h],
                size: [rw, tab_bar_height - half_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: colors.bg,
                bg_color: colors.bg,
                corner_radius: 0.0,
            });
        }
    }
}

/// Build horizontal regions for the tab bar background.
///
/// Returns a list of `(x, width, is_active)` tuples covering the full rect width.
/// When an active range is present, the bar is split into up to three regions:
/// before-active (inactive), the active tab, and after-active (inactive).
fn build_bg_regions(rect: Rect, active_range: Option<(f32, f32)>) -> Vec<(f32, f32, bool)> {
    let left = rect.x;
    let right = rect.x + rect.width;

    match active_range {
        Some((xa, xb)) => {
            let mut regions = Vec::with_capacity(3);
            if xa > left {
                regions.push((left, xa - left, false));
            }
            regions.push((xa, (xb - xa).min(right - xa), true));
            if xb < right {
                regions.push((xb, right - xb, false));
            }
            regions
        }
        None => vec![(left, rect.width, false)],
    }
}

/// Height of the bottom separator line in pixels.
const SEPARATOR_HEIGHT: f32 = 1.0;

/// Columns reserved for the gear icon: one space + one glyph.
const GEAR_RESERVED_COLS: usize = 2;

/// Columns reserved for the equalize icon: one space + one glyph.
const EQUALIZE_RESERVED_COLS: usize = 2;
/// Tab bar layout never needs more than this many grid units, which keeps the
/// integer-to-float conversion exact for pixel placement.
const MAX_RENDER_GRID_UNITS: usize = 65_535;

/// Build a 1px separator line at the bottom of a pane's tab bar area.
///
/// Gives a clear visual boundary between the tab bar and terminal content.
/// `skip_range` is an optional pixel X range to leave undrawn (used to omit the separator
/// beneath the active tab so it appears raised).
pub struct TabBarSeparatorContext<'a> {
    pub out: &'a mut Vec<CellInstance>,
    pub rect: Rect,
    pub cell_w: f32,
    pub color: [f32; 4],
    pub tab_bar_height: f32,
    pub skip_range: Option<(f32, f32)>,
}

pub fn build_tab_bar_separator(ctx: TabBarSeparatorContext<'_>) {
    let TabBarSeparatorContext { out, rect, cell_w, color, tab_bar_height, skip_range } = ctx;
    if cell_w <= 0.0 {
        return;
    }

    let separator_y = rect.y + tab_bar_height - SEPARATOR_HEIGHT;
    let cols = columns_in_width(rect.width, cell_w);

    for col_idx in 0..cols {
        let x = rect.x + columns_to_pixels(col_idx, cell_w);
        if let Some((xa, xb)) = skip_range {
            if x + cell_w > xa && x < xb {
                continue;
            }
        }
        out.push(CellInstance {
            pos: [x, separator_y],
            size: [cell_w, SEPARATOR_HEIGHT],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: color,
            bg_color: color,
            corner_radius: 0.0,
        });
    }

    // Fill the fractional-pixel remainder at the right edge.
    let remainder = rect.width - columns_to_pixels(cols, cell_w);
    if remainder > 0.0 {
        let x = rect.x + columns_to_pixels(cols, cell_w);
        let skip = skip_range.is_some_and(|(xa, xb)| x + remainder > xa && x < xb);
        if !skip {
            out.push(CellInstance {
                pos: [x, separator_y],
                size: [remainder, SEPARATOR_HEIGHT],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg_color: color,
                bg_color: color,
                corner_radius: 0.0,
            });
        }
    }
}

/// Compute the tab bar height in pixels for a workspace with the given parameters.
///
/// Accounts for multi-row stacking when there are more tabs than fit in one row.
#[derive(Clone, Copy)]
pub struct TabBarHeightRequest {
    pub tab_count: usize,
    pub ws_width: f32,
    pub tab_width_chars: u16,
    pub cell_w: f32,
    pub row_height: f32,
    pub badge_cols: usize,
}

pub fn compute_tab_bar_height(request: TabBarHeightRequest) -> f32 {
    let TabBarHeightRequest {
        tab_count,
        ws_width,
        tab_width_chars,
        cell_w,
        row_height,
        badge_cols,
    } = request;
    if cell_w <= 0.0 || row_height <= 0.0 {
        return row_height.max(1.0);
    }
    let gear_cols: usize = GEAR_RESERVED_COLS;
    let total_cols = columns_in_width(ws_width, cell_w);
    let available = total_cols.saturating_sub(badge_cols).saturating_sub(gear_cols);
    let tab_w = usize::from(tab_width_chars).max(1);
    let tabs_per_row = (available / tab_w).max(1);
    let effective_count = tab_count.max(1);
    let rows = effective_count.div_ceil(tabs_per_row);
    columns_to_pixels(rows, row_height)
}

/// Compute the number of columns occupied by the workspace badge.
///
/// Returns 0 when no badge is shown (single workspace or unnamed workspace).
/// Badge layout: space + name + space + gap ≈ `name_len` + 4 columns.
pub fn badge_columns(ws_name: Option<&str>, show_badge: bool) -> usize {
    match (show_badge, ws_name) {
        (true, Some(n)) => n.chars().count() + 4,
        _ => 0,
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
    /// The inner `Option<[f32; 4]>` is `None` for unnamed workspaces (no colored pill).
    pub badge: Option<(String, Option<[f32; 4]>)>,
    /// Whether the active tab in this workspace has multiple panes.
    pub has_multiple_panes: bool,
    /// Pre-computed tab bar height for this workspace (accounts for multi-row stacking).
    pub tab_bar_height: f32,
    /// Pixel X range `(start, end)` of the active tab on row 0. `None` when no active tab is on
    /// row 0 or the workspace has no tabs.
    pub active_tab_pixel_range: Option<(f32, f32)>,
}

/// Parameters for building tab bar text instances.
pub struct TabBarTextParams<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    /// Tab data for each tab in the workspace.
    pub tabs: &'a [TabData],
    /// Workspace badge: `(workspace_name, accent_color)`. `None` when single workspace.
    /// The inner `Option<[f32; 4]>` is `None` for unnamed workspaces (no colored pill).
    pub badge: Option<(&'a str, Option<[f32; 4]>)>,
    /// Whether to render the gear icon on the far right.
    pub show_gear: bool,
    /// Whether to render the equalize icon left of the gear.
    pub show_equalize: bool,
    pub colors: &'a TabBarColors,
    /// Closure that resolves a character to atlas UV coordinates.
    /// Returns `(uv_min, uv_max)`. `FnMut` because atlas rasterization
    /// may occur for uncached glyphs.
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
    /// Tab bar height in pixels (from config).
    pub tab_bar_height: f32,
    /// AI indicator bar height in pixels (from config).
    pub indicator_height: f32,
    /// Fixed tab width in characters (includes leading/trailing padding).
    pub tab_width: u16,
    /// Which tab's close button is shown (hovered). `None` = no hover.
    pub hovered_tab_close: Option<usize>,
    /// Which tab is hovered (for background highlight). `None` = no hover.
    pub hovered_tab: Option<usize>,
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
    /// Column range `(start_col, end_col)` of the active tab on row 0. `None` when no active tab
    /// is on row 0 (e.g. active tab is on row 1+ in a multi-row bar).
    pub active_tab_col_range: Option<(usize, usize)>,
    /// Tooltip hover targets for truncated tab titles.
    pub tooltip_targets: Vec<TooltipAnchor>,
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
                active_tab_col_range: None,
                tooltip_targets: Vec::new(),
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
        active_tab_col_range: None,
        tooltip_targets: Vec::new(),
    };

    // Reserve columns for the gear icon on the far right (2 cols: space + gear).
    let gear_cols: usize = if params.show_gear { GEAR_RESERVED_COLS } else { 0 };
    // Reserve columns for the equalize icon left of gear (2 cols: space + icon).
    let equalize_cols: usize = if params.show_equalize { EQUALIZE_RESERVED_COLS } else { 0 };
    let content_cols = max_cols.saturating_sub(gear_cols).saturating_sub(equalize_cols);

    // Render workspace badge if present.
    {
        let mut writer = TabBarWriter { instances: &mut instances, params, max_cols: content_cols };
        if let Some((ws_name, accent_color)) = writer.params.badge {
            col = writer.render_badge(col, ws_name, accent_color);
        }

        // Render tab labels (and indicator bars for tabs with active AI state).
        col = render_tabs(&mut writer, &mut hit_targets, col);
    }

    {
        let mut writer = TabBarWriter { instances: &mut instances, params, max_cols };
        // Render equalize icon left of gear if requested.
        if writer.params.show_equalize {
            col = writer.render_equalize(&mut hit_targets, col, gear_cols);
        }

        // Render gear icon on the far right if requested.
        if writer.params.show_gear {
            writer.render_gear(&mut hit_targets, col);
        }
    }

    (instances, hit_targets)
}

struct TabBarWriter<'a, 'b> {
    instances: &'a mut Vec<CellInstance>,
    params: &'a mut TabBarTextParams<'b>,
    max_cols: usize,
}

impl TabBarWriter<'_, '_> {
    fn cell_w(&self) -> f32 {
        self.params.cell_size.0
    }

    /// Emit a single character instance at the given column, returning the next
    /// column index.
    fn emit_char(&mut self, ch: char, col: usize, fg: [f32; 4], bg: [f32; 4]) -> usize {
        if col >= self.max_cols {
            return col;
        }

        let (cell_w, cell_h) = self.params.cell_size;
        let x = self.params.rect.x + columns_to_pixels(col, cell_w);
        let y = self.params.rect.y + ((self.params.tab_bar_height - cell_h) / 2.0).max(0.0);
        let (uv_min, uv_max) = (self.params.resolve_glyph)(ch);

        self.instances.push(CellInstance {
            pos: [x, y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: fg,
            bg_color: bg,
            corner_radius: 0.0,
        });

        col + 1
    }

    /// Render the workspace badge: accent-coloured cell (leading space + name + trailing space)
    /// followed by a 16px gap.
    fn render_badge(
        &mut self,
        mut col: usize,
        ws_name: &str,
        accent_color: Option<[f32; 4]>,
    ) -> usize {
        let bg = self.params.colors.bg;

        if let Some(accent) = accent_color {
            // Named workspace: render accent-coloured pill with high-contrast text.
            let pill_start_col = col;
            let pill_char_count = 1 + ws_name.chars().count() + 1;
            let pill_bg = [accent[0], accent[1], accent[2], 0.25];

            let pill_width = columns_to_pixels(pill_char_count, self.cell_w());
            let pill_x = self.params.rect.x + columns_to_pixels(pill_start_col, self.cell_w());
            self.instances.push(solid_quad(
                pill_x,
                self.params.rect.y,
                pill_width,
                self.params.tab_bar_height,
                pill_bg,
            ));

            // Use high-contrast active text color for readability on the pill.
            let text_fg = self.params.colors.active_text;
            col = self.emit_char(' ', col, text_fg, pill_bg);
            for ch in ws_name.chars() {
                col = self.emit_char(ch, col, text_fg, pill_bg);
            }
            col = self.emit_char(' ', col, text_fg, pill_bg);
        } else {
            // Unnamed workspace: render plain muted text, no pill background.
            let text_fg = self.params.colors.text;
            col = self.emit_char(' ', col, text_fg, bg);
            for ch in ws_name.chars() {
                col = self.emit_char(ch, col, text_fg, bg);
            }
            col = self.emit_char(' ', col, text_fg, bg);
        }

        // 16px gap after badge (approximately 2 cell widths), reverting to normal bg.
        let gap = gap_columns(16.0, self.cell_w());
        for _ in 0..gap {
            col = self.emit_char(' ', col, self.params.colors.text, bg);
        }

        col
    }

    /// Render the equalize icon (⊞) just left of the gear icon's reserved space.
    fn render_equalize(
        &mut self,
        hit_targets: &mut TabBarHitTargets,
        mut col: usize,
        gear_cols: usize,
    ) -> usize {
        // Need at least 2 cols for equalize + gear reserved space.
        if self.max_cols < 2 {
            return col;
        }

        let bg = self.params.colors.bg;

        // Equalize icon sits at the rightmost column before the gear's reserved space.
        let equalize_col = self.max_cols.saturating_sub(gear_cols).saturating_sub(1);

        // Fill gap between last tab and equalize icon with background.
        while col < equalize_col {
            col = self.emit_char(' ', col, self.params.colors.text, bg);
        }

        let equalize_start_col = col;
        col = self.emit_char('\u{229E}', col, self.params.colors.text, bg);

        let equalize_x = self.params.rect.x + columns_to_pixels(equalize_start_col, self.cell_w());
        let equalize_width =
            columns_to_pixels(col.saturating_sub(equalize_start_col), self.cell_w());
        hit_targets.equalize_rect = Some(Rect {
            x: equalize_x,
            y: self.params.rect.y,
            width: equalize_width,
            height: self.params.tab_bar_height,
        });

        col
    }

    /// Render the gear icon on the far right of the tab bar.
    fn render_gear(&mut self, hit_targets: &mut TabBarHitTargets, mut col: usize) {
        if self.max_cols < 2 {
            return;
        }

        let bg = self.params.colors.bg;

        // Position the gear at the rightmost column.
        let gear_col = self.max_cols - 1;

        // Fill gap between last tab and gear with background.
        while col < gear_col {
            col = self.emit_char(' ', col, self.params.colors.text, bg);
        }

        let gear_start_col = col;
        col = self.emit_char('\u{2699}', col, self.params.colors.text, bg);

        let gear_x = self.params.rect.x + columns_to_pixels(gear_start_col, self.cell_w());
        let gear_width = columns_to_pixels(col.saturating_sub(gear_start_col), self.cell_w());
        hit_targets.gear_rect = Some(Rect {
            x: gear_x,
            y: self.params.rect.y,
            width: gear_width,
            height: self.params.tab_bar_height,
        });
    }
}

/// Render tab labels with hit targets for click handling, plus AI indicator
/// bars underneath tabs that have an active AI state.
///
/// Tabs have a fixed width (`params.tab_width` columns) and wrap to new rows
/// when they would exceed `max_cols`. Returns the column where content ends
/// on row 0 (for gear/equalize icon positioning).
fn render_tabs(
    writer: &mut TabBarWriter<'_, '_>,
    hit_targets: &mut TabBarHitTargets,
    start_col: usize,
) -> usize {
    let bg = writer.params.colors.bg;
    let tab_w = usize::from(writer.params.tab_width).max(1);

    // Compute the single-row height from total height / number of rows.
    let tab_count = writer.params.tabs.len().max(1);
    let available = writer.max_cols.saturating_sub(start_col);
    let tabs_per_row = (available / tab_w).max(1);
    let num_rows = tab_count.div_ceil(tabs_per_row);
    let row_height = if num_rows > 0 {
        writer.params.tab_bar_height / f32::from(render_grid_units(num_rows))
    } else {
        writer.params.tab_bar_height
    };

    // Save original params that we temporarily mutate per row.
    let base_y = writer.params.rect.y;
    let total_tab_bar_h = writer.params.tab_bar_height;
    let mut ctx = TabRenderContext {
        writer,
        hit_targets,
        tab_w,
        row_height,
        base_y,
        start_col,
        bg,
        row: 0,
        col: start_col,
        row0_end_col: start_col,
    };

    let tabs: Vec<TabData> = ctx.writer.params.tabs.to_vec();
    for (tab_idx, tab) in tabs.iter().enumerate() {
        render_tab(&mut ctx, tab_idx, tab);
    }

    // Restore original params.
    ctx.writer.params.rect.y = base_y;
    ctx.writer.params.tab_bar_height = total_tab_bar_h;

    // Return row 0 end column so gear/equalize render correctly in row 0.
    ctx.row0_end_col
}

struct TabRenderContext<'a, 'b, 'c> {
    writer: &'a mut TabBarWriter<'b, 'c>,
    hit_targets: &'a mut TabBarHitTargets,
    tab_w: usize,
    row_height: f32,
    base_y: f32,
    start_col: usize,
    bg: [f32; 4],
    row: usize,
    col: usize,
    row0_end_col: usize,
}

impl TabRenderContext<'_, '_, '_> {
    fn cell_w(&self) -> f32 {
        self.writer.cell_w()
    }

    fn max_cols(&self) -> usize {
        self.writer.max_cols
    }

    fn emit_char(&mut self, ch: char, fg: [f32; 4], bg: [f32; 4]) {
        self.col = self.writer.emit_char(ch, self.col, fg, bg);
    }

    fn record_hit_targets(
        &mut self,
        tab_idx: usize,
        tab: &TabData,
        tab_rect: Rect,
        is_truncated: bool,
    ) {
        let close_x = tab_rect.x + tab_rect.width - (2.0 * self.cell_w());
        self.hit_targets.close_rects.push((
            tab_idx,
            Rect { x: close_x, y: tab_rect.y, width: 2.0 * self.cell_w(), height: self.row_height },
        ));

        self.hit_targets.tab_rects.push((tab_idx, tab_rect));
        if is_truncated {
            self.hit_targets
                .tooltip_targets
                .push(TooltipAnchor { text: tab.title.clone(), rect: tab_rect });
        }
    }

    fn render_indicator_bar(
        &mut self,
        indicator_color: [f32; 4],
        tab_start_col: usize,
        row_base_y: f32,
    ) {
        let tab_x = self.writer.params.rect.x + columns_to_pixels(tab_start_col, self.cell_w());
        let tab_width = columns_to_pixels(self.tab_w, self.cell_w());
        self.writer.instances.push(solid_quad(
            tab_x,
            row_base_y,
            tab_width,
            self.writer.params.indicator_height,
            indicator_color,
        ));
    }
}

fn render_grid_units(units: usize) -> u16 {
    u16::try_from(units).unwrap_or(u16::MAX)
}

fn columns_to_pixels(columns: usize, unit: f32) -> f32 {
    f32::from(render_grid_units(columns)) * unit
}

fn render_tab(ctx: &mut TabRenderContext<'_, '_, '_>, tab_idx: usize, tab: &TabData) {
    if tab_idx > 0 && ctx.col + ctx.tab_w > ctx.max_cols() {
        ctx.row += 1;
        ctx.col = ctx.start_col;
    }

    let fg = if tab.is_active {
        ctx.writer.params.colors.active_text
    } else {
        ctx.writer.params.colors.text
    };
    let is_hovered = !tab.is_active && ctx.writer.params.hovered_tab == Some(tab_idx);
    let tab_bg = if tab.is_active {
        ctx.writer.params.colors.active_bg
    } else if is_hovered {
        [ctx.bg[0] + 0.04, ctx.bg[1] + 0.04, ctx.bg[2] + 0.04, ctx.bg[3]]
    } else {
        ctx.bg
    };
    let tab_start_col = ctx.col;
    let row_base_y = ctx.base_y + columns_to_pixels(ctx.row, ctx.row_height);

    if tab.is_active && ctx.row == 0 {
        ctx.hit_targets.active_tab_col_range = Some((tab_start_col, tab_start_col + ctx.tab_w));
    }

    ctx.writer.params.rect.y = row_base_y;
    ctx.writer.params.tab_bar_height = ctx.row_height;

    let (display_title, is_truncated, show_close) =
        tab_display_title(tab, ctx.tab_w, ctx.writer.params.hovered_tab_close == Some(tab_idx));

    let tab_start_instance = ctx.writer.instances.len();
    ctx.emit_char(' ', fg, tab_bg);
    for &ch in &display_title {
        ctx.emit_char(ch, fg, tab_bg);
    }
    if show_close {
        ctx.emit_char(' ', fg, tab_bg);
        ctx.emit_char('\u{00D7}', fg, tab_bg);
    } else {
        ctx.emit_char(' ', fg, tab_bg);
    }

    let expected_end = tab_start_col + ctx.tab_w;
    while ctx.col < expected_end.min(ctx.max_cols()) {
        ctx.emit_char(' ', fg, tab_bg);
    }
    ctx.col = expected_end.min(ctx.max_cols());

    render_tab_separator(ctx, tab_idx, tab, tab_start_col, row_base_y);
    render_tab_indicator(ctx, tab, tab_start_col, row_base_y);
    let tab_rect = Rect {
        x: ctx.writer.params.rect.x + columns_to_pixels(tab_start_col, ctx.cell_w()),
        y: row_base_y,
        width: columns_to_pixels(ctx.tab_w, ctx.cell_w()),
        height: ctx.row_height,
    };
    ctx.record_hit_targets(tab_idx, tab, tab_rect, is_truncated);

    if ctx.row == 0 {
        ctx.row0_end_col = ctx.col;
    }

    let tab_offset = tab_slide_offset(ctx, tab_idx, tab_start_col);
    if tab_offset != 0.0 {
        apply_x_offset(ctx.writer.instances, tab_start_instance, tab_offset);
    }
    if ctx.writer.params.dragging_tab == Some(tab_idx) {
        render_drag_underline(ctx, tab_start_col, row_base_y, tab_offset);
    }
}

fn tab_display_title(tab: &TabData, tab_w: usize, show_close: bool) -> (Vec<char>, bool, bool) {
    let available_title = if tab_w >= 4 {
        if show_close && tab_w >= 6 { tab_w.saturating_sub(4) } else { tab_w.saturating_sub(2) }
    } else {
        tab_w.saturating_sub(2)
    };

    let title_chars: Vec<char> = tab.title.chars().collect();
    let is_truncated = tab_w >= 4 && title_chars.len() > available_title;
    let display_title: Vec<char> = if is_truncated {
        let keep = available_title.saturating_sub(1);
        let mut t: Vec<char> = title_chars.get(..keep).map_or_else(Vec::new, <[char]>::to_vec);
        t.push('\u{2026}');
        t
    } else {
        let mut t = title_chars;
        while t.len() < available_title {
            t.push(' ');
        }
        t
    };

    (display_title, is_truncated, show_close)
}

fn render_tab_separator(
    ctx: &mut TabRenderContext<'_, '_, '_>,
    tab_idx: usize,
    tab: &TabData,
    tab_start_col: usize,
    row_base_y: f32,
) {
    let next_is_inactive = ctx.writer.params.tabs.get(tab_idx + 1).is_some_and(|t| !t.is_active);
    if !tab.is_active && next_is_inactive && ctx.writer.params.dragging_tab != Some(tab_idx) {
        let sep_x =
            ctx.writer.params.rect.x + columns_to_pixels(tab_start_col + ctx.tab_w, ctx.cell_w());
        ctx.writer.instances.push(CellInstance {
            pos: [sep_x - 1.0, row_base_y],
            size: [1.0, ctx.row_height],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg_color: ctx.writer.params.colors.separator,
            bg_color: ctx.writer.params.colors.separator,
            corner_radius: 0.0,
        });
    }
}

fn render_tab_indicator(
    ctx: &mut TabRenderContext<'_, '_, '_>,
    tab: &TabData,
    tab_start_col: usize,
    row_base_y: f32,
) {
    if let Some(indicator_color) = tab.ai_indicator {
        ctx.render_indicator_bar(indicator_color, tab_start_col, row_base_y);
    }
}

fn tab_slide_offset(
    ctx: &TabRenderContext<'_, '_, '_>,
    tab_idx: usize,
    tab_start_col: usize,
) -> f32 {
    let tab_x = ctx.writer.params.rect.x + columns_to_pixels(tab_start_col, ctx.cell_w());
    if ctx.writer.params.dragging_tab == Some(tab_idx) {
        ctx.writer.params.drag_cursor_x - ctx.writer.params.drag_grab_offset - tab_x
    } else {
        ctx.writer.params.tab_offsets.get(tab_idx).copied().unwrap_or(0.0)
    }
}

fn render_drag_underline(
    ctx: &mut TabRenderContext<'_, '_, '_>,
    tab_start_col: usize,
    row_base_y: f32,
    tab_offset: f32,
) {
    let underline_height = 2.0;
    let underline_y = row_base_y + ctx.row_height - underline_height;
    let visual_x =
        ctx.writer.params.rect.x + columns_to_pixels(tab_start_col, ctx.cell_w()) + tab_offset;
    let underline_width = columns_to_pixels(ctx.tab_w, ctx.cell_w());
    ctx.writer.instances.push(solid_quad(
        visual_x,
        underline_y,
        underline_width,
        underline_height,
        ctx.writer.params.accent_color,
    ));
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

/// Calculate how many cell-width columns fit in a given pixel width.
fn columns_in_width(width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || !width.is_finite() || width <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_RENDER_GRID_UNITS && columns_to_pixels(high, cell_w) <= width {
        low = high;
        high = high.saturating_mul(2).min(MAX_RENDER_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if columns_to_pixels(mid, cell_w) <= width {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

/// Calculate how many columns a pixel gap requires.
fn gap_columns(gap_px: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || !gap_px.is_finite() || gap_px <= 0.0 {
        return 0;
    }

    let floor_cols = columns_in_width(gap_px, cell_w);
    if columns_to_pixels(floor_cols, cell_w) < gap_px {
        floor_cols.saturating_add(1).min(MAX_RENDER_GRID_UNITS)
    } else {
        floor_cols
    }
}

/// Maximum fraction of pane width that the title pill may occupy.
pub const PILL_MAX_WIDTH_FRACTION: f32 = 0.3;

/// Build a semi-transparent title pill in the top-right corner of a pane.
///
/// Only call this when the active tab has 2+ panes. The pill is positioned at
/// the first terminal content line (`pane_rect.y` + `tab_bar_height` for top-edge
/// panes, or `pane_rect.y` otherwise). Its height is exactly one cell height.
/// Title is truncated with `…` if it exceeds 30% of the pane width.
pub struct PaneTitlePillContext<'a, 'b> {
    pub out: &'a mut Vec<CellInstance>,
    pub title: &'b str,
    pub pane_rect: Rect,
    pub tab_bar_height: f32,
    pub cell_size: (f32, f32),
    pub colors: &'b TabBarColors,
    pub resolve_glyph: &'a mut GlyphResolver<'a>,
}

pub fn build_pane_title_pill(ctx: PaneTitlePillContext<'_, '_>) {
    let PaneTitlePillContext {
        out,
        title,
        pane_rect,
        tab_bar_height,
        cell_size,
        colors,
        resolve_glyph,
    } = ctx;
    let (cell_w, cell_h) = cell_size;
    if cell_w <= 0.0 || cell_h <= 0.0 {
        return;
    }

    // Pill height is exactly one cell height.
    let pill_h = cell_h;

    // Maximum chars allowed (30% of pane width, minus 2 padding chars).
    let pane_cols = columns_in_width(pane_rect.width, cell_w);
    let max_content_cols = pane_cols.saturating_mul(3).saturating_div(10).saturating_sub(2);
    if max_content_cols == 0 {
        return;
    }

    // Build the display string (truncate with ellipsis if needed).
    let chars: Vec<char> = title.chars().collect();
    let (display_chars, truncated): (Vec<char>, bool) = if chars.len() <= max_content_cols {
        (chars, false)
    } else {
        // Reserve 1 slot for the ellipsis.
        let truncated_len = max_content_cols.saturating_sub(1);
        let mut tc: Vec<char> = chars.into_iter().take(truncated_len).collect();
        tc.push('\u{2026}'); // …
        (tc, true)
    };
    let _ = truncated; // used only to construct the display string

    // Pill width: 1 padding + content + 1 padding.
    let content_len = display_chars.len();
    let pill_cols = content_len + 2;
    let pill_width = columns_to_pixels(pill_cols, cell_w);

    // X position: inset by 1 cell from the right edge of the pane.
    let pill_x = (pane_rect.x + pane_rect.width - pill_width - cell_w).max(pane_rect.x);
    let pill_y = pane_rect.y + tab_bar_height;

    // Semi-transparent background.
    let pill_bg = [colors.bg[0], colors.bg[1], colors.bg[2], 0.7];
    out.push(solid_quad(pill_x, pill_y, pill_width, pill_h, pill_bg));

    // Text: vertically centred within pill_h.
    let text_y = pill_y + ((pill_h - cell_h) / 2.0).max(0.0);
    let text_color = colors.text;

    // Leading padding space (no glyph needed — background covers it).
    // Emit each content character.
    for (i, &ch) in display_chars.iter().enumerate() {
        // +1 to skip the leading padding column.
        let char_x = pill_x + columns_to_pixels(i + 1, cell_w);
        let (uv_min, uv_max) = resolve_glyph(ch);
        out.push(CellInstance {
            pos: [char_x, text_y],
            size: [0.0, 0.0],
            uv_min,
            uv_max,
            fg_color: text_color,
            bg_color: [0.0; 4],
            corner_radius: 0.0,
        });
    }
}
