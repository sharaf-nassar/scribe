pub mod atlas;
pub mod box_drawing;
pub mod palette;
pub mod pipeline;
pub mod types;

use std::collections::HashMap;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::cell::Flags;
use cosmic_text::CacheKey;
use scribe_common::config::CursorShape;
use wgpu::{Device, Queue, TextureFormat};

use crate::atlas::{FontParams, GlyphAtlas, GlyphKey, ShapedRunGlyph};
use crate::palette::ColorPalette;
use crate::pipeline::{PipelineConfig, TerminalPipeline};
use crate::types::{CellInstance, CellSize, GridSize};

/// Dimming factor applied to foreground when the DIM flag is set.
const DIM_FACTOR: f32 = 0.67;

/// A cell collected from `display_iter` for the ligature pre-pass.
struct CollectedCell {
    point: alacritty_terminal::index::Point,
    c: char,
    fg: alacritty_terminal::vte::ansi::Color,
    bg: alacritty_terminal::vte::ansi::Color,
    flags: Flags,
}

/// A contiguous run of same-styled characters on one row.
struct StyledRun {
    line: i32,
    start_col: usize,
    text: String,
    bold: bool,
    italic: bool,
}

/// Info about a cell that participates in a ligature.
struct LigatureCellInfo {
    cache_key: CacheKey,
    glyph_span: u8,
    cell_index: u8,
}

/// Map from grid position `(line, column)` to ligature info.
type LigatureMap = HashMap<(i32, usize), LigatureCellInfo>;

/// GPU-accelerated terminal renderer.
///
/// Wires together the glyph atlas, colour palette, and wgpu render pipeline
/// to draw a terminal grid from alacritty-terminal state.
pub struct TerminalRenderer {
    atlas: GlyphAtlas,
    pipeline: TerminalPipeline,
    palette: ColorPalette,
    cell_size: CellSize,
    grid_size: GridSize,
    viewport_size: (u32, u32),
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    default_fg_dim: [f32; 4],
    cursor_shape: CursorShape,
    cursor_color: [f32; 4],
}

impl TerminalRenderer {
    /// Create a new renderer.
    ///
    /// `params` controls font family, size, weight, ligatures, and line
    /// padding; `viewport_size` is the surface dimensions in physical pixels.
    pub fn new(
        device: &Device,
        queue: &Queue,
        surface_format: TextureFormat,
        params: &FontParams,
        viewport_size: (u32, u32),
    ) -> Self {
        let atlas = GlyphAtlas::new(device, queue, params);
        let cell_size = atlas.cell_size();
        let grid_size = compute_grid_size(viewport_size, cell_size);
        let palette = ColorPalette::new();

        let pipeline = TerminalPipeline::new(&PipelineConfig {
            device,
            queue,
            surface_format,
            atlas_view: atlas.texture_view(),
            atlas_sampler: atlas.sampler(),
            viewport_size,
            cell_size: (cell_size.width, cell_size.height),
        });

        Self {
            atlas,
            pipeline,
            palette,
            cell_size,
            grid_size,
            viewport_size,
            default_fg: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
            default_bg: srgb_to_linear_rgba([0.0, 0.0, 0.0, 1.0]),
            default_fg_dim: {
                let fg = srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]);
                [
                    fg.first().copied().unwrap_or(0.0) * DIM_FACTOR,
                    fg.get(1).copied().unwrap_or(0.0) * DIM_FACTOR,
                    fg.get(2).copied().unwrap_or(0.0) * DIM_FACTOR,
                    fg.get(3).copied().unwrap_or(1.0),
                ]
            },
            cursor_shape: CursorShape::Block,
            cursor_color: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
        }
    }

    /// Return the current grid dimensions (columns x rows).
    pub const fn grid_size(&self) -> GridSize {
        self.grid_size
    }

    /// Return the measured cell size from the font.
    pub const fn cell_size(&self) -> CellSize {
        self.cell_size
    }

    /// Build cell instances for this terminal with a pixel offset.
    ///
    /// Each instance position is translated by `offset` so the pane can
    /// be rendered at an arbitrary position within the viewport. The
    /// returned instances should be collected into a single buffer and
    /// drawn in one render pass.
    ///
    /// `cursor_visible` controls whether the cursor overlay is rendered.
    /// Pass `false` during a blink-off phase; pass `true` otherwise.
    #[allow(
        clippy::too_many_arguments,
        reason = "public API needs device, queue, term, offset, and cursor_visible — cannot reduce"
    )]
    pub fn build_instances_at<T: EventListener>(
        &mut self,
        device: &Device,
        queue: &Queue,
        term: &mut Term<T>,
        offset: (f32, f32),
        cursor_visible: bool,
    ) -> Vec<CellInstance> {
        {
            let _damage = term.damage();
        }

        let content = term.renderable_content();
        let cursor_point = content.cursor.point;

        // Respect the terminal's own cursor visibility (SHOW_CURSOR / DECTCEM).
        // When applications hide the cursor (e.g. during TUI redraws),
        // `content.cursor.shape` is `Hidden` — we must not draw it even if
        // the blink timer says visible.
        let term_cursor_shown =
            content.cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden;
        let effective_cursor_visible = cursor_visible && term_cursor_shown;

        let instances = self.build_instances_offset(
            device,
            queue,
            content,
            cursor_point,
            offset,
            effective_cursor_visible,
        );

        term.reset_damage();
        instances
    }

    /// Handle a viewport resize.
    ///
    /// Returns the new grid dimensions so the caller can send a resize
    /// event to the PTY server.
    pub fn resize(&mut self, queue: &Queue, new_size: (u32, u32)) -> GridSize {
        self.viewport_size = new_size;
        self.grid_size = compute_grid_size(new_size, self.cell_size);
        self.pipeline.update_viewport(
            queue,
            new_size,
            (self.cell_size.width, self.cell_size.height),
        );
        self.grid_size
    }

    /// Return a mutable reference to the pipeline.
    pub fn pipeline_mut(&mut self) -> &mut TerminalPipeline {
        &mut self.pipeline
    }

    /// Return the current default background color (for use as clear color).
    pub const fn default_bg(&self) -> [f32; 4] {
        self.default_bg
    }

    /// Apply a theme, updating the palette and default colors.
    ///
    /// Theme colors are sRGB; we convert to linear for the GPU pipeline
    /// (the sRGB framebuffer applies the inverse transform on output).
    pub fn set_theme(&mut self, theme: &scribe_common::theme::Theme) {
        self.default_fg = srgb_to_linear_rgba(theme.foreground);
        self.default_bg = srgb_to_linear_rgba(theme.background);
        self.cursor_color = srgb_to_linear_rgba(theme.cursor);
        let linear_fg = self.default_fg;
        self.default_fg_dim = [
            linear_fg.first().copied().unwrap_or(0.0) * DIM_FACTOR,
            linear_fg.get(1).copied().unwrap_or(0.0) * DIM_FACTOR,
            linear_fg.get(2).copied().unwrap_or(0.0) * DIM_FACTOR,
            linear_fg.get(3).copied().unwrap_or(1.0),
        ];
        let mut linear_ansi = [[0.0_f32; 4]; 16];
        for (i, color) in theme.ansi_colors.iter().enumerate() {
            if let Some(dest) = linear_ansi.get_mut(i) {
                *dest = srgb_to_linear_rgba(*color);
            }
        }
        self.palette.override_ansi(&linear_ansi);
    }

    /// Set the cursor shape used when rendering the terminal cursor.
    pub fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }

    /// Resolve a single character to atlas UV coordinates.
    ///
    /// Returns `(uv_min, uv_max)`. Blank characters (space, NUL) return
    /// zeroed UVs.  The glyph is rasterised and cached on first use.
    pub fn resolve_glyph(
        &mut self,
        device: &Device,
        queue: &Queue,
        ch: char,
    ) -> ([f32; 2], [f32; 2]) {
        if ch == ' ' || ch == '\u{0}' {
            return ([0.0, 0.0], [0.0, 0.0]);
        }
        let key = GlyphKey { c: ch, bold: false, italic: false };
        let entry = self.atlas.get_or_insert(device, queue, key);
        (entry.uv_min, entry.uv_max)
    }

    /// Rebuild the glyph atlas with new font parameters.
    ///
    /// This is synchronous and may cause a frame skip. It also rebuilds
    /// the pipeline bind group so the new atlas texture is used for rendering
    /// (fixes a latent bug where `rebuild_atlas` created a new texture but
    /// left the pipeline referencing the old one).
    pub fn rebuild_atlas(&mut self, device: &Device, queue: &Queue, params: &FontParams) {
        self.atlas = GlyphAtlas::new(device, queue, params);
        self.cell_size = self.atlas.cell_size();
        self.grid_size = compute_grid_size(self.viewport_size, self.cell_size);
        self.pipeline.rebuild_bind_group(device, self.atlas.texture_view(), self.atlas.sampler());
        self.pipeline.update_viewport(
            queue,
            self.viewport_size,
            (self.cell_size.width, self.cell_size.height),
        );
    }

    /// Build the per-cell instance buffer with a pixel offset applied to
    /// every instance position.
    #[allow(
        clippy::too_many_arguments,
        reason = "internal method needs all render context parameters plus offset and cursor state"
    )]
    #[allow(
        clippy::cast_precision_loss,
        reason = "grid coordinates are small (< 2^16) and fit exactly in f32"
    )]
    fn build_instances_offset(
        &mut self,
        device: &Device,
        queue: &Queue,
        content: alacritty_terminal::term::RenderableContent<'_>,
        cursor_point: alacritty_terminal::index::Point,
        offset: (f32, f32),
        cursor_visible: bool,
    ) -> Vec<CellInstance> {
        let cell_w = self.cell_size.width;
        let cell_h = self.cell_size.height;

        // When the user scrolls into history, display_iter yields cells with
        // negative line indices (e.g. Line(-5) for display_offset=5).  Add the
        // offset back so screen row 0 maps to the top of the content area
        // rather than bleeding above the pane.
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "display_offset is bounded by scrollback_lines (≤ 100_000), fits in i32"
        )]
        let line_offset = content.display_offset as i32;

        // Collect cells: display_iter is a one-shot iterator.
        let cells: Vec<CollectedCell> = content
            .display_iter
            .map(|indexed| CollectedCell {
                point: indexed.point,
                c: indexed.cell.c,
                fg: indexed.cell.fg,
                bg: indexed.cell.bg,
                flags: indexed.cell.flags,
            })
            .collect();

        // Run ligature pre-pass when the atlas has ligatures enabled.
        let ligature_map = if self.atlas.ligatures() {
            let runs = detect_styled_runs(&cells);
            build_ligature_map(&runs, &mut self.atlas)
        } else {
            LigatureMap::new()
        };
        let use_ligatures = !ligature_map.is_empty();

        // Beam/underline cursors push an extra overlay quad — allow some headroom.
        let estimated_capacity =
            usize::from(self.grid_size.cols) * usize::from(self.grid_size.rows) + 1;
        let mut instances = Vec::with_capacity(estimated_capacity);

        for cell in &cells {
            // Skip spacer cells that follow wide characters.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let col = cell.point.column.0 as f32;
            let row = (cell.point.line.0 + line_offset) as f32;
            let pos = [col * cell_w + offset.0, row * cell_h + offset.1];

            let (fg, bg) = self.resolve_cell_colors_raw(cell.fg, cell.bg, cell.flags);

            // Look up glyph UV in the atlas.
            let (uv_min, uv_max) = if use_ligatures {
                self.resolve_glyph_uv_for_collected(device, queue, cell, &ligature_map)
            } else {
                self.resolve_glyph_uv_raw(device, queue, cell.c, cell.flags)
            };

            let is_cursor = cursor_visible
                && cell.point.line == cursor_point.line
                && cell.point.column == cursor_point.column;

            if is_cursor {
                self.push_cursor_instances(
                    &mut instances,
                    pos,
                    cell_w,
                    cell_h,
                    uv_min,
                    uv_max,
                    fg,
                    bg,
                );
            } else {
                instances.push(CellInstance {
                    pos,
                    size: [0.0, 0.0],
                    uv_min,
                    uv_max,
                    fg_color: fg,
                    bg_color: bg,
                });
            }
        }

        instances
    }

    /// Push one or more instances for the cursor cell, based on the cursor shape.
    #[allow(
        clippy::too_many_arguments,
        reason = "cursor rendering needs position, dimensions, UVs, and both cell colors"
    )]
    fn push_cursor_instances(
        &self,
        instances: &mut Vec<CellInstance>,
        pos: [f32; 2],
        cell_w: f32,
        cell_h: f32,
        uv_min: [f32; 2],
        uv_max: [f32; 2],
        fg: [f32; 4],
        bg: [f32; 4],
    ) {
        match self.cursor_shape {
            CursorShape::Block => {
                // Invert fg/bg for the whole cell.
                instances.push(CellInstance {
                    pos,
                    size: [0.0, 0.0],
                    uv_min,
                    uv_max,
                    fg_color: bg,
                    bg_color: fg,
                });
            }
            CursorShape::Beam => {
                // Normal cell first.
                instances.push(CellInstance {
                    pos,
                    size: [0.0, 0.0],
                    uv_min,
                    uv_max,
                    fg_color: fg,
                    bg_color: bg,
                });
                // Thin vertical bar overlay.
                let beam_w = f32::max(2.0, cell_w / 8.0);
                instances.push(CellInstance {
                    pos,
                    size: [beam_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg_color: self.cursor_color,
                    bg_color: self.cursor_color,
                });
            }
            CursorShape::Underline => {
                // Normal cell first.
                instances.push(CellInstance {
                    pos,
                    size: [0.0, 0.0],
                    uv_min,
                    uv_max,
                    fg_color: fg,
                    bg_color: bg,
                });
                // Thin horizontal bar at the bottom of the cell.
                let ul_h = f32::max(2.0, cell_h / 8.0);
                instances.push(CellInstance {
                    pos: [
                        pos.first().copied().unwrap_or(0.0),
                        pos.get(1).copied().unwrap_or(0.0) + cell_h - ul_h,
                    ],
                    size: [cell_w, ul_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg_color: self.cursor_color,
                    bg_color: self.cursor_color,
                });
            }
        }
    }

    /// Resolve foreground and background colours for a cell, applying
    /// INVERSE, HIDDEN, and DIM flags.
    #[allow(dead_code, reason = "retained for callers that work with &Cell directly")]
    fn resolve_cell_colors(
        &self,
        cell: &alacritty_terminal::term::cell::Cell,
    ) -> ([f32; 4], [f32; 4]) {
        let mut fg = self.resolve_color(cell.fg);
        let mut bg = self.resolve_color(cell.bg);

        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        if cell.flags.contains(Flags::DIM) {
            apply_dim(&mut fg);
        }

        (fg, bg)
    }

    /// Look up the glyph UV coordinates, returning zeroed UVs for blank cells.
    #[allow(dead_code, reason = "retained for callers that work with &Cell directly")]
    fn resolve_glyph_uv(
        &mut self,
        device: &Device,
        queue: &Queue,
        cell: &alacritty_terminal::term::cell::Cell,
    ) -> ([f32; 2], [f32; 2]) {
        if cell.c == ' ' || cell.c == '\u{0}' {
            return ([0.0, 0.0], [0.0, 0.0]);
        }

        let key = GlyphKey {
            c: cell.c,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
        };
        let entry = self.atlas.get_or_insert(device, queue, key);
        (entry.uv_min, entry.uv_max)
    }

    /// Resolve foreground and background colours from raw fields, applying
    /// INVERSE, HIDDEN, and DIM flags.
    fn resolve_cell_colors_raw(
        &self,
        fg_color: alacritty_terminal::vte::ansi::Color,
        bg_color: alacritty_terminal::vte::ansi::Color,
        flags: Flags,
    ) -> ([f32; 4], [f32; 4]) {
        let mut fg = self.resolve_color(fg_color);
        let mut bg = self.resolve_color(bg_color);

        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        if flags.contains(Flags::DIM) {
            apply_dim(&mut fg);
        }

        (fg, bg)
    }

    /// Resolve glyph UV from raw char and flags, returning zeroed UVs for
    /// blank cells.
    fn resolve_glyph_uv_raw(
        &mut self,
        device: &Device,
        queue: &Queue,
        c: char,
        flags: Flags,
    ) -> ([f32; 2], [f32; 2]) {
        if c == ' ' || c == '\u{0}' {
            return ([0.0, 0.0], [0.0, 0.0]);
        }

        let key = GlyphKey {
            c,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
        };
        let entry = self.atlas.get_or_insert(device, queue, key);
        (entry.uv_min, entry.uv_max)
    }

    /// Resolve glyph UV for a collected cell, checking ligature map first.
    ///
    /// If the cell is found in the ligature map, the shaped glyph is looked
    /// up (or rasterised) via the atlas shaped cache. For multi-cell ligatures
    /// the UV is split horizontally so each cell renders its portion of the
    /// wider glyph.
    #[allow(
        clippy::cast_precision_loss,
        reason = "glyph_span and cell_index are small integers that fit exactly in f32"
    )]
    fn resolve_glyph_uv_for_collected(
        &mut self,
        device: &Device,
        queue: &Queue,
        cell: &CollectedCell,
        ligature_map: &LigatureMap,
    ) -> ([f32; 2], [f32; 2]) {
        if cell.c == ' ' || cell.c == '\u{0}' {
            return ([0.0, 0.0], [0.0, 0.0]);
        }

        // Box-drawing and block elements are rendered procedurally by the
        // atlas (in `rasterize_rgba`). Skip the ligature map so they are
        // never served via the shaped-glyph path, which would bypass the
        // procedural renderer.
        if crate::box_drawing::is_box_drawing(cell.c) {
            return self.resolve_glyph_uv_raw(device, queue, cell.c, cell.flags);
        }

        if let Some(info) = ligature_map.get(&(cell.point.line.0, cell.point.column.0)) {
            let entry = self.atlas.get_or_insert_shaped(queue, info.cache_key, info.glyph_span);

            if info.glyph_span > 1 {
                // Split UV horizontally: each cell gets its slice of the
                // wider glyph texture.
                let uv_width = entry.uv_max[0] - entry.uv_min[0];
                let span = f32::from(info.glyph_span);
                let idx = f32::from(info.cell_index);
                let slice_min = entry.uv_min[0] + uv_width * idx / span;
                let slice_max = entry.uv_min[0] + uv_width * (idx + 1.0) / span;
                ([slice_min, entry.uv_min[1]], [slice_max, entry.uv_max[1]])
            } else {
                (entry.uv_min, entry.uv_max)
            }
        } else {
            self.resolve_glyph_uv_raw(device, queue, cell.c, cell.flags)
        }
    }

    /// Resolve an alacritty colour to RGBA floats, using sensible defaults
    /// for semantic colours (Foreground, Background, etc.).
    fn resolve_color(&self, color: alacritty_terminal::vte::ansi::Color) -> [f32; 4] {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};

        match color {
            Color::Named(
                NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor,
            ) => self.default_fg,
            Color::Named(NamedColor::Background) => self.default_bg,
            Color::Named(NamedColor::DimForeground) => self.default_fg_dim,
            other => self.palette.resolve(other),
        }
    }
}

/// Compute grid dimensions from viewport size and cell size.
///
/// Returns at least 1 column and 1 row.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "viewport / cell_size yields a small positive value that fits in u16"
)]
fn compute_grid_size(viewport: (u32, u32), cell: CellSize) -> GridSize {
    let cols =
        if cell.width > 0.0 { (f32::from(viewport.0 as u16) / cell.width) as u16 } else { 1 };
    let rows =
        if cell.height > 0.0 { (f32::from(viewport.1 as u16) / cell.height) as u16 } else { 1 };
    GridSize { cols: cols.max(1), rows: rows.max(1) }
}

/// Apply the DIM effect: multiply RGB by [`DIM_FACTOR`], leave alpha unchanged.
fn apply_dim(color: &mut [f32; 4]) {
    if let Some(r) = color.get_mut(0) {
        *r *= DIM_FACTOR;
    }
    if let Some(g) = color.get_mut(1) {
        *g *= DIM_FACTOR;
    }
    if let Some(b) = color.get_mut(2) {
        *b *= DIM_FACTOR;
    }
}

/// Convert a single sRGB channel to linear space.
#[allow(
    clippy::suboptimal_flops,
    reason = "clarity over micro-optimisation for the standard sRGB transfer function"
)]
fn srgb_channel_to_linear(s: f32) -> f32 {
    if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
}

/// Convert an sRGB `[f32; 4]` colour to linear space (alpha unchanged).
///
/// Use this for any sRGB colors (e.g. theme colors) that will be passed
/// to the GPU pipeline, which expects linear colors.
pub fn srgb_to_linear_rgba(c: [f32; 4]) -> [f32; 4] {
    [
        srgb_channel_to_linear(c.first().copied().unwrap_or(0.0)),
        srgb_channel_to_linear(c.get(1).copied().unwrap_or(0.0)),
        srgb_channel_to_linear(c.get(2).copied().unwrap_or(0.0)),
        c.get(3).copied().unwrap_or(1.0),
    ]
}

/// Accumulator state for the run currently being built in `detect_styled_runs`.
struct RunAccum {
    line: i32,
    start_col: usize,
    text: String,
    bold: bool,
    italic: bool,
    /// Foreground colour of the first cell in the run.
    foreground: Option<alacritty_terminal::vte::ansi::Color>,
    /// Background colour of the first cell in the run.
    background: Option<alacritty_terminal::vte::ansi::Color>,
}

impl RunAccum {
    fn new() -> Self {
        Self {
            line: 0,
            start_col: 0,
            text: String::new(),
            bold: false,
            italic: false,
            foreground: None,
            background: None,
        }
    }

    /// Flush into `out` if the accumulated text has two or more characters.
    ///
    /// The text buffer is always cleared afterwards.
    fn flush(&mut self, out: &mut Vec<StyledRun>) {
        if self.text.chars().count() >= 2 {
            out.push(StyledRun {
                line: self.line,
                start_col: self.start_col,
                text: std::mem::take(&mut self.text),
                bold: self.bold,
                italic: self.italic,
            });
        } else {
            self.text.clear();
        }
    }

    /// Reset the accumulator to start a new run from `cell`.
    fn reset(&mut self, cell: &CollectedCell) {
        self.line = cell.point.line.0;
        self.start_col = cell.point.column.0;
        self.bold = cell.flags.contains(Flags::BOLD);
        self.italic = cell.flags.contains(Flags::ITALIC);
        self.foreground = Some(cell.fg);
        self.background = Some(cell.bg);
        self.text.push(cell.c);
    }

    /// Whether `cell` continues the current run (same style, row, and adjacent column).
    fn matches(&self, cell: &CollectedCell) -> bool {
        let expected_col = self.start_col + self.text.chars().count();
        cell.point.column.0 == expected_col
            && self.line == cell.point.line.0
            && self.bold == cell.flags.contains(Flags::BOLD)
            && self.italic == cell.flags.contains(Flags::ITALIC)
            && self.foreground.is_some_and(|fg| fg == cell.fg)
            && self.background.is_some_and(|bg| bg == cell.bg)
    }
}

/// Group collected cells into contiguous same-styled runs suitable for
/// ligature shaping.
///
/// Wide-character spacer cells are skipped. Runs spanning a row change or a
/// style change (bold, italic, fg, bg) are flushed. Only runs with two or
/// more characters are returned, since a single character cannot form a
/// ligature.
fn detect_styled_runs(cells: &[CollectedCell]) -> Vec<StyledRun> {
    let mut out: Vec<StyledRun> = Vec::new();
    let mut accum = RunAccum::new();

    for cell in cells {
        // Skip wide-char spacer glyphs — they carry no printable content.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }

        if accum.text.is_empty() {
            accum.reset(cell);
        } else if accum.matches(cell) {
            accum.text.push(cell.c);
        } else {
            accum.flush(&mut out);
            accum.reset(cell);
        }
    }

    // Flush the final run.
    if !accum.text.is_empty() {
        accum.flush(&mut out);
    }

    out
}

/// Insert all spanned columns for a multi-cell ligature glyph into `map`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "span_idx iterates 0..glyph_span (u8), so always fits in u8"
)]
fn insert_multi_cell_glyph(map: &mut LigatureMap, line: i32, col: usize, glyph: &ShapedRunGlyph) {
    for span_idx in 0..usize::from(glyph.glyph_span) {
        map.insert(
            (line, col + span_idx),
            LigatureCellInfo {
                cache_key: glyph.cache_key,
                glyph_span: glyph.glyph_span,
                cell_index: span_idx as u8,
            },
        );
    }
}

/// Context for checking whether a single-cell glyph is a contextual alternate.
struct SingleGlyphCtx<'a> {
    map: &'a mut LigatureMap,
    atlas: &'a mut GlyphAtlas,
    run: &'a StyledRun,
    chars: &'a [char],
}

impl SingleGlyphCtx<'_> {
    /// Insert the glyph at `(line, col)` only if it is a contextual alternate.
    ///
    /// A glyph is a contextual alternate when its run-shaped `CacheKey`
    /// differs from the key produced by shaping the character in isolation.
    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "bold and italic are font variant flags, not control flow bools"
    )]
    fn insert_if_alternate(&mut self, col: usize, glyph: &ShapedRunGlyph) {
        let ch = self.chars.get(glyph.col_offset).copied();
        let is_alternate = ch.is_some_and(|c| {
            self.atlas
                .shape_single_cache_key(c, self.run.bold, self.run.italic)
                .is_some_and(|solo_key| solo_key != glyph.cache_key)
        });
        if is_alternate {
            self.map.insert(
                (self.run.line, col),
                LigatureCellInfo { cache_key: glyph.cache_key, glyph_span: 1, cell_index: 0 },
            );
        }
    }
}

/// Build a map from `(line, column)` to [`LigatureCellInfo`] for every cell
/// that participates in a ligature or contextual alternate.
///
/// For each [`StyledRun`], the run is shaped as a whole. Glyphs that span
/// more than one column (`glyph_span > 1`) are always recorded. Single-cell
/// glyphs are only recorded when their shaped `CacheKey` differs from the
/// key produced by shaping the character in isolation — this detects
/// contextual alternates without wasting atlas space on unchanged glyphs.
fn build_ligature_map(runs: &[StyledRun], atlas: &mut GlyphAtlas) -> LigatureMap {
    let mut map = LigatureMap::new();

    for run in runs {
        let shaped = atlas.shape_run(&run.text, run.bold, run.italic).to_vec();
        let chars: Vec<char> = run.text.chars().collect();

        for glyph in &shaped {
            let col = run.start_col + glyph.col_offset;
            if glyph.glyph_span > 1 {
                insert_multi_cell_glyph(&mut map, run.line, col, glyph);
            } else {
                let mut ctx = SingleGlyphCtx { map: &mut map, atlas, run, chars: &chars };
                ctx.insert_if_alternate(col, glyph);
            }
        }
    }

    map
}
