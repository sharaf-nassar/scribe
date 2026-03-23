pub mod atlas;
pub mod palette;
pub mod pipeline;
pub mod types;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::cell::Flags;
use wgpu::{Device, Queue, TextureFormat};

use crate::atlas::{GlyphAtlas, GlyphKey};
use crate::palette::ColorPalette;
use crate::pipeline::{PipelineConfig, TerminalPipeline};
use crate::types::{CellInstance, CellSize, GridSize};

/// Foreground colour used when the palette returns a semantic colour
/// (Foreground, Cursor, etc.) that has no indexed mapping.
const DEFAULT_FG: [f32; 4] = [0.8, 0.8, 0.8, 1.0];

/// Background colour matching the render-pass clear colour.
const DEFAULT_BG: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Dimming factor applied to foreground when the DIM flag is set.
const DIM_FACTOR: f32 = 0.67;

/// Pre-computed dimmed foreground (avoids computing per-cell).
const DEFAULT_FG_DIM: [f32; 4] = [
    DEFAULT_FG[0] * DIM_FACTOR,
    DEFAULT_FG[1] * DIM_FACTOR,
    DEFAULT_FG[2] * DIM_FACTOR,
    DEFAULT_FG[3],
];

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
}

impl TerminalRenderer {
    /// Create a new renderer.
    ///
    /// `font_size` controls glyph rasterisation; `viewport_size` is the
    /// surface dimensions in physical pixels.
    pub fn new(
        device: &Device,
        queue: &Queue,
        surface_format: TextureFormat,
        font_size: f32,
        viewport_size: (u32, u32),
    ) -> Self {
        let atlas = GlyphAtlas::new(device, queue, font_size);
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

        Self { atlas, pipeline, palette, cell_size, grid_size, viewport_size }
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
    pub fn build_instances_at<T: EventListener>(
        &mut self,
        device: &Device,
        queue: &Queue,
        term: &mut Term<T>,
        offset: (f32, f32),
    ) -> Vec<CellInstance> {
        {
            let _damage = term.damage();
        }

        let content = term.renderable_content();
        let cursor_point = content.cursor.point;

        let instances = self.build_instances_offset(device, queue, content, cursor_point, offset);

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

    /// Build the per-cell instance buffer with a pixel offset applied to
    /// every instance position.
    #[allow(
        clippy::too_many_arguments,
        reason = "internal method needs all render context parameters plus offset"
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
    ) -> Vec<CellInstance> {
        let cell_w = self.cell_size.width;
        let cell_h = self.cell_size.height;

        let estimated_capacity =
            usize::from(self.grid_size.cols) * usize::from(self.grid_size.rows);
        let mut instances = Vec::with_capacity(estimated_capacity);

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = &indexed.cell;

            // Skip spacer cells that follow wide characters.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let col = point.column.0 as f32;
            let row = point.line.0 as f32;
            let pos = [col * cell_w + offset.0, row * cell_h + offset.1];

            let (fg, bg) = self.resolve_cell_colors(cell);

            // Handle cursor: invert fg/bg at the cursor position (block cursor).
            let is_cursor = point.line == cursor_point.line && point.column == cursor_point.column;
            let (fg, bg) = if is_cursor { (bg, fg) } else { (fg, bg) };

            // Look up glyph UV in the atlas.
            let (uv_min, uv_max) = self.resolve_glyph_uv(device, queue, cell);

            instances.push(CellInstance { pos, uv_min, uv_max, fg_color: fg, bg_color: bg });
        }

        instances
    }

    /// Resolve foreground and background colours for a cell, applying
    /// INVERSE, HIDDEN, and DIM flags.
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

    /// Resolve an alacritty colour to RGBA floats, using sensible defaults
    /// for semantic colours (Foreground, Background, etc.).
    fn resolve_color(&self, color: alacritty_terminal::vte::ansi::Color) -> [f32; 4] {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};

        match color {
            Color::Named(
                NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor,
            ) => DEFAULT_FG,
            Color::Named(NamedColor::Background) => DEFAULT_BG,
            Color::Named(NamedColor::DimForeground) => DEFAULT_FG_DIM,
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
