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

/// Dimming factor applied to foreground when the DIM flag is set.
const DIM_FACTOR: f32 = 0.67;

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

    /// Rebuild the glyph atlas with a new font size.
    /// This is synchronous and may cause a frame skip.
    pub fn rebuild_atlas(&mut self, device: &Device, queue: &Queue, font_size: f32) {
        self.atlas = GlyphAtlas::new(device, queue, font_size);
        self.cell_size = self.atlas.cell_size();
        self.grid_size = compute_grid_size(self.viewport_size, self.cell_size);
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
