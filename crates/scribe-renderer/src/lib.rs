pub mod atlas;
pub mod box_drawing;
pub mod chrome;
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

use crate::atlas::{FontParams, GlyphAtlas, GlyphKey, GlyphStyle, ShapedRunGlyph};
use crate::palette::ColorPalette;
use crate::pipeline::{PipelineConfig, TerminalPipeline};
use crate::types::{CellInstance, CellSize, GridSize};

/// Dimming factor applied to foreground when the DIM flag is set.
const DIM_FACTOR: f32 = 0.67;
/// Grid coordinates in the renderer stay within this range, which keeps
/// integer-to-float conversion exact for pixel placement.
const MAX_RENDER_GRID_UNITS: usize = 65_535;

fn render_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_RENDER_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn render_grid_pixels(units: usize, cell_size: f32) -> f32 {
    f32::from(render_grid_units(units)) * cell_size
}

fn render_units_in_extent(extent: f32, cell_size: f32) -> u16 {
    if cell_size <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_RENDER_GRID_UNITS && render_grid_pixels(high, cell_size) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_RENDER_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if render_grid_pixels(mid, cell_size) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    render_grid_units(low)
}

fn viewport_axis_f32(axis: u32) -> f32 {
    f32::from(u16::try_from(axis).unwrap_or(u16::MAX))
}

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

#[derive(Clone, Copy)]
pub struct RenderResources<'a> {
    pub device: &'a Device,
    pub queue: &'a Queue,
}

#[derive(Clone, Copy)]
pub struct TerminalRenderOptions {
    pub offset: (f32, f32),
    pub cursor_visible: bool,
}

#[derive(Clone, Copy)]
struct CursorRenderState {
    point: alacritty_terminal::index::Point,
    visible: bool,
}

#[derive(Clone, Copy)]
struct RenderGeometry {
    line_offset: i32,
    cell_size: CellSize,
    offset: (f32, f32),
}

enum LigatureRenderMode<'a> {
    Disabled,
    Enabled(&'a LigatureMap),
}

struct CellInstanceBuildRequest<'a> {
    resources: RenderResources<'a>,
    cells: &'a [CollectedCell],
    ligatures: LigatureRenderMode<'a>,
    geometry: RenderGeometry,
    cursor: CursorRenderState,
    estimated_capacity: usize,
}

#[derive(Clone, Copy)]
struct CursorInstanceRequest {
    pos: [f32; 2],
    cell_size: CellSize,
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    fg: [f32; 4],
    bg: [f32; 4],
}

#[derive(Clone, Copy)]
struct CollectedCellKey {
    c: char,
    flags: Flags,
    line: i32,
    column: usize,
}

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
    default_bright_fg: [f32; 4],
    default_bg: [f32; 4],
    default_fg_dim: [f32; 4],
    cursor_shape: CursorShape,
    cursor_color: [f32; 4],
    selection_bg: [f32; 4],
    selection_fg: [f32; 4],
    /// Reusable buffer for cells collected from `display_iter`, cleared and
    /// refilled each frame to avoid per-frame heap allocation.
    cell_scratch: Vec<CollectedCell>,
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
            default_bright_fg: srgb_to_linear_rgba(boost_srgb_brightness([0.8, 0.8, 0.8, 1.0])),
            default_bg: srgb_to_linear_rgba([0.0, 0.0, 0.0, 1.0]),
            default_fg_dim: {
                // Apply DIM in sRGB space: multiply the sRGB value by DIM_FACTOR,
                // then convert to linear for the GPU pipeline.
                let srgb = [0.8_f32, 0.8, 0.8, 1.0];
                [
                    srgb_channel_to_linear(srgb[0] * DIM_FACTOR),
                    srgb_channel_to_linear(srgb[1] * DIM_FACTOR),
                    srgb_channel_to_linear(srgb[2] * DIM_FACTOR),
                    srgb[3],
                ]
            },
            cursor_shape: CursorShape::Block,
            cursor_color: srgb_to_linear_rgba([0.8, 0.8, 0.8, 1.0]),
            selection_bg: srgb_to_linear_rgba([0.25, 0.25, 0.28, 1.0]),
            selection_fg: srgb_to_linear_rgba([1.0, 1.0, 1.0, 1.0]),
            cell_scratch: Vec::new(),
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
    pub fn build_instances_at<T: EventListener>(
        &mut self,
        resources: RenderResources<'_>,
        term: &mut Term<T>,
        options: TerminalRenderOptions,
    ) -> Vec<CellInstance> {
        let content = term.renderable_content();
        let cursor =
            CursorRenderState { point: content.cursor.point, visible: options.cursor_visible };

        // Respect the terminal's own cursor visibility (SHOW_CURSOR / DECTCEM).
        // When applications hide the cursor (e.g. during TUI redraws),
        // `content.cursor.shape` is `Hidden` — we must not draw it even if
        // the blink timer says visible.
        let term_cursor_shown =
            content.cursor.shape != alacritty_terminal::vte::ansi::CursorShape::Hidden;
        let effective_cursor_visible = cursor.visible && term_cursor_shown;
        let effective_cursor = CursorRenderState { visible: effective_cursor_visible, ..cursor };

        // Honour the terminal's DECSCUSR cursor shape when a TUI app has
        // explicitly changed it (e.g. Claude Code sets Beam for its input
        // field).  Fall back to the user's config shape when the terminal
        // reports the default (Block) — this lets `cursor_shape = "beam"`
        // in config.toml work for normal shell usage.
        let saved_shape = self.cursor_shape;
        if effective_cursor_visible {
            match content.cursor.shape {
                alacritty_terminal::vte::ansi::CursorShape::Beam => {
                    self.cursor_shape = CursorShape::Beam;
                }
                alacritty_terminal::vte::ansi::CursorShape::Underline => {
                    self.cursor_shape = CursorShape::Underline;
                }
                _ => {}
            }
        }

        let instances =
            self.build_instances_offset(resources, content, effective_cursor, options.offset);

        self.cursor_shape = saved_shape;
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
        self.default_bright_fg = srgb_to_linear_rgba(boost_srgb_brightness(theme.foreground));
        self.default_bg = srgb_to_linear_rgba(theme.background);
        self.cursor_color = srgb_to_linear_rgba(theme.cursor);
        // Apply DIM in sRGB space: use the raw sRGB theme foreground values,
        // multiply by DIM_FACTOR, then convert to linear for the GPU pipeline.
        let srgb_fg = theme.foreground;
        self.default_fg_dim = [
            srgb_channel_to_linear(srgb_fg.first().copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_channel_to_linear(srgb_fg.get(1).copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_channel_to_linear(srgb_fg.get(2).copied().unwrap_or(0.0) * DIM_FACTOR),
            srgb_fg.get(3).copied().unwrap_or(1.0),
        ];
        let mut linear_ansi = [[0.0_f32; 4]; 16];
        for (i, color) in theme.ansi_colors.iter().enumerate() {
            if let Some(dest) = linear_ansi.get_mut(i) {
                *dest = srgb_to_linear_rgba(*color);
            }
        }
        self.palette.override_ansi(&linear_ansi);
        self.selection_bg = srgb_to_linear_rgba(theme.selection);
        self.selection_fg = srgb_to_linear_rgba(theme.selection_foreground);
    }

    /// Return the current selection background color (linear space).
    pub const fn selection_bg(&self) -> [f32; 4] {
        self.selection_bg
    }

    /// Return the current selection foreground color (linear space).
    pub const fn selection_fg(&self) -> [f32; 4] {
        self.selection_fg
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
        // The atlas texture was replaced: UV coordinates in any cached instance
        // data are now stale.  Force a full GPU re-upload on the next frame.
        self.pipeline.invalidate_instance_cache();
        self.pipeline.update_viewport(
            queue,
            self.viewport_size,
            (self.cell_size.width, self.cell_size.height),
        );
    }

    /// Build the per-cell instance buffer with a pixel offset applied to
    /// every instance position.
    fn build_instances_offset(
        &mut self,
        resources: RenderResources<'_>,
        content: alacritty_terminal::term::RenderableContent<'_>,
        cursor: CursorRenderState,
        offset: (f32, f32),
    ) -> Vec<CellInstance> {
        let cell_size = self.cell_size;

        // When the user scrolls into history, display_iter yields cells with
        // negative line indices (e.g. Line(-5) for display_offset=5).  Add the
        // offset back so screen row 0 maps to the top of the content area
        // rather than bleeding above the pane.
        let line_offset = i32::try_from(content.display_offset).unwrap_or(i32::MAX);

        // Collect cells: display_iter is a one-shot iterator.  Reuse the
        // scratch buffer to avoid a heap allocation every frame.  We take it
        // out of `self` so the borrow checker lets us call `&mut self` methods
        // freely inside the loop; the vec is returned to `self` afterwards.
        self.cell_scratch.clear();
        self.cell_scratch.extend(content.display_iter.map(|indexed| CollectedCell {
            point: indexed.point,
            c: indexed.cell.c,
            fg: indexed.cell.fg,
            bg: indexed.cell.bg,
            flags: indexed.cell.flags,
        }));
        let cells = std::mem::take(&mut self.cell_scratch);

        // Run ligature pre-pass when the atlas has ligatures enabled.
        let ligature_map = if self.atlas.ligatures() {
            let runs = detect_styled_runs(&cells);
            build_ligature_map(&runs, &mut self.atlas)
        } else {
            LigatureMap::new()
        };
        let ligatures = if ligature_map.is_empty() {
            LigatureRenderMode::Disabled
        } else {
            LigatureRenderMode::Enabled(&ligature_map)
        };

        // Beam/underline cursors push an extra overlay quad — allow some headroom.
        let estimated_capacity =
            usize::from(self.grid_size.cols) * usize::from(self.grid_size.rows) + 1;
        let instances = self.build_cell_instances(CellInstanceBuildRequest {
            resources,
            cells: &cells,
            ligatures,
            geometry: RenderGeometry { line_offset, cell_size, offset },
            cursor,
            estimated_capacity,
        });

        // Return the scratch buffer so it retains its allocation for the next frame.
        self.cell_scratch = cells;
        instances
    }

    /// Build the instance list from a collected-cell slice.
    ///
    /// Factored out of `build_instances_offset` so that method stays under the
    /// 80-line limit while the cell slice can be passed as a plain reference
    /// (no borrow conflict with `self`).
    fn build_cell_instances(&mut self, request: CellInstanceBuildRequest<'_>) -> Vec<CellInstance> {
        let CellInstanceBuildRequest {
            resources,
            cells,
            ligatures,
            geometry,
            cursor,
            estimated_capacity,
        } = request;

        let mut instances = Vec::with_capacity(estimated_capacity);
        for cell in cells {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let col = f32::from(render_grid_units(cell.point.column.0));
            let row_index = usize::try_from(cell.point.line.0.saturating_add(geometry.line_offset))
                .unwrap_or(0);
            let row = f32::from(render_grid_units(row_index));
            let pos = [
                col * geometry.cell_size.width + geometry.offset.0,
                row * geometry.cell_size.height + geometry.offset.1,
            ];
            let (fg, bg) = self.resolve_cell_colors_raw(cell.fg, cell.bg, cell.flags);
            let (uv_min, uv_max) = match ligatures {
                LigatureRenderMode::Enabled(ligature_map) => self
                    .resolve_glyph_uv_for_collected_fields(
                        resources,
                        CollectedCellKey {
                            c: cell.c,
                            flags: cell.flags,
                            line: cell.point.line.0,
                            column: cell.point.column.0,
                        },
                        ligature_map,
                    ),
                LigatureRenderMode::Disabled => {
                    self.resolve_glyph_uv_raw(resources.device, resources.queue, cell.c, cell.flags)
                }
            };
            let is_cursor = cursor.visible
                && cell.point.line == cursor.point.line
                && cell.point.column == cursor.point.column;
            if is_cursor {
                self.push_cursor_instances(
                    &mut instances,
                    CursorInstanceRequest {
                        pos,
                        cell_size: geometry.cell_size,
                        uv_min,
                        uv_max,
                        fg,
                        bg,
                    },
                );
            } else {
                instances.push(CellInstance {
                    pos,
                    size: [0.0, 0.0],
                    uv_min,
                    uv_max,
                    fg_color: fg,
                    bg_color: bg,
                    corner_radius: 0.0,
                });
            }
        }
        instances
    }

    /// Push one or more instances for the cursor cell, based on the cursor shape.
    fn push_cursor_instances(
        &self,
        instances: &mut Vec<CellInstance>,
        request: CursorInstanceRequest,
    ) {
        let CursorInstanceRequest { pos, cell_size, uv_min, uv_max, fg, bg } = request;
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
                    corner_radius: 0.0,
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
                    corner_radius: 0.0,
                });
                // Thin vertical bar overlay.
                let beam_w = f32::max(2.0, cell_size.width / 8.0);
                instances.push(CellInstance {
                    pos,
                    size: [beam_w, cell_size.height],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg_color: self.cursor_color,
                    bg_color: self.cursor_color,
                    corner_radius: 0.0,
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
                    corner_radius: 0.0,
                });
                // Thin horizontal bar at the bottom of the cell.
                let ul_h = f32::max(2.0, cell_size.height / 8.0);
                instances.push(CellInstance {
                    pos: [
                        pos.first().copied().unwrap_or(0.0),
                        pos.get(1).copied().unwrap_or(0.0) + cell_size.height - ul_h,
                    ],
                    size: [cell_size.width, ul_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg_color: self.cursor_color,
                    bg_color: self.cursor_color,
                    corner_radius: 0.0,
                });
            }
        }
    }

    /// Resolve foreground and background colours from raw fields, applying
    /// BOLD-bright, INVERSE, HIDDEN, and DIM flags.
    fn resolve_cell_colors_raw(
        &self,
        fg_color: alacritty_terminal::vte::ansi::Color,
        bg_color: alacritty_terminal::vte::ansi::Color,
        flags: Flags,
    ) -> ([f32; 4], [f32; 4]) {
        let effective_fg =
            if flags.contains(Flags::BOLD) { bold_to_bright(fg_color) } else { fg_color };
        let mut fg = self.resolve_color(effective_fg);
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

        let style =
            GlyphStyle { bold: flags.contains(Flags::BOLD), italic: flags.contains(Flags::ITALIC) };
        let key = GlyphKey { c, bold: style.bold, italic: style.italic };
        let entry = self.atlas.get_or_insert(device, queue, key);
        (entry.uv_min, entry.uv_max)
    }

    /// Resolve glyph UV for a cell, checking ligature map first.
    ///
    /// Accepts individual cell fields by value so the caller is not required
    /// to hold a borrow on `self.cell_scratch` while calling this `&mut self`
    /// method. If the cell is found in the ligature map, the shaped glyph is
    /// looked up (or rasterised) via the atlas shaped cache. For multi-cell
    /// ligatures the UV is split horizontally so each cell renders its portion
    /// of the wider glyph.
    fn resolve_glyph_uv_for_collected_fields(
        &mut self,
        resources: RenderResources<'_>,
        cell: CollectedCellKey,
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
            return self.resolve_glyph_uv_raw(
                resources.device,
                resources.queue,
                cell.c,
                cell.flags,
            );
        }

        if let Some(info) = ligature_map.get(&(cell.line, cell.column)) {
            let entry =
                self.atlas.get_or_insert_shaped(resources.queue, info.cache_key, info.glyph_span);

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
            self.resolve_glyph_uv_raw(resources.device, resources.queue, cell.c, cell.flags)
        }
    }

    /// Resolve an alacritty colour to RGBA floats, using sensible defaults
    /// for semantic colours (Foreground, Background, etc.).
    fn resolve_color(&self, color: alacritty_terminal::vte::ansi::Color) -> [f32; 4] {
        use alacritty_terminal::vte::ansi::{Color, NamedColor};

        match color {
            Color::Named(NamedColor::Foreground | NamedColor::Cursor) => self.default_fg,
            Color::Named(NamedColor::BrightForeground) => self.default_bright_fg,
            Color::Named(NamedColor::Background) => self.default_bg,
            Color::Named(NamedColor::DimForeground) => self.default_fg_dim,
            other => self.palette.resolve(other),
        }
    }
}

/// Map a foreground colour to its bright variant when the BOLD flag is set.
///
/// Standard terminal behaviour: ANSI colours 0-7 (named or indexed) are
/// promoted to their bright equivalents 8-15, and the semantic `Foreground`
/// is promoted to `BrightForeground`.  RGB / 256-colour values and already-
/// bright colours are returned unchanged.
fn bold_to_bright(
    color: alacritty_terminal::vte::ansi::Color,
) -> alacritty_terminal::vte::ansi::Color {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    match color {
        Color::Named(named) => Color::Named(match named {
            NamedColor::Black => NamedColor::BrightBlack,
            NamedColor::Red => NamedColor::BrightRed,
            NamedColor::Green => NamedColor::BrightGreen,
            NamedColor::Yellow => NamedColor::BrightYellow,
            NamedColor::Blue => NamedColor::BrightBlue,
            NamedColor::Magenta => NamedColor::BrightMagenta,
            NamedColor::Cyan => NamedColor::BrightCyan,
            NamedColor::White => NamedColor::BrightWhite,
            NamedColor::Foreground => NamedColor::BrightForeground,
            other => other,
        }),
        Color::Indexed(idx @ 0..=7) => Color::Indexed(idx + 8),
        other => other,
    }
}

/// Boost an sRGB colour toward full brightness for the bold-bright foreground.
///
/// Each channel is pushed 30 % of the way toward 1.0, so dim themes gain a
/// noticeable bump while themes near white stay clamped.
fn boost_srgb_brightness(srgb: [f32; 4]) -> [f32; 4] {
    const FACTOR: f32 = 0.30;
    [
        srgb[0] + (1.0 - srgb[0]) * FACTOR,
        srgb[1] + (1.0 - srgb[1]) * FACTOR,
        srgb[2] + (1.0 - srgb[2]) * FACTOR,
        srgb[3],
    ]
}

/// Compute grid dimensions from viewport size and cell size.
///
/// Returns at least 1 column and 1 row.
fn compute_grid_size(viewport: (u32, u32), cell: CellSize) -> GridSize {
    let viewport_width = viewport_axis_f32(viewport.0);
    let viewport_height = viewport_axis_f32(viewport.1);
    let cols =
        if cell.width > 0.0 { render_units_in_extent(viewport_width, cell.width) } else { 1 };
    let rows =
        if cell.height > 0.0 { render_units_in_extent(viewport_height, cell.height) } else { 1 };
    GridSize { cols: cols.max(1), rows: rows.max(1) }
}

/// Apply the DIM effect in sRGB space, then convert back to linear.
///
/// Terminal convention applies DIM by multiplying sRGB channel values by
/// [`DIM_FACTOR`].  Because our pipeline stores linear colours, we round-trip
/// through sRGB so the perceptual result matches other terminal emulators.
fn apply_dim(color: &mut [f32; 4]) {
    // DIM is conventionally applied in sRGB space.  Since our pipeline stores
    // linear colours, we convert each channel back to sRGB, apply the factor,
    // and convert to linear again.
    for c in color.get_mut(..3).into_iter().flatten() {
        let srgb = linear_to_srgb_channel(*c);
        *c = srgb_channel_to_linear(srgb * DIM_FACTOR);
    }
}

/// Convert a single linear channel to sRGB space.
///
/// This is the inverse of [`srgb_channel_to_linear`].
fn linear_to_srgb_channel(l: f32) -> f32 {
    if l <= 0.003_130_8 { l * 12.92 } else { l.powf(1.0 / 2.4).mul_add(1.055, -0.055) }
}

/// Convert a single sRGB channel to linear space.
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
        let expected_col = self.start_col
            + self
                .text
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>();
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
fn insert_multi_cell_glyph(map: &mut LigatureMap, line: i32, col: usize, glyph: &ShapedRunGlyph) {
    for span_idx in 0..usize::from(glyph.glyph_span) {
        map.insert(
            (line, col + span_idx),
            LigatureCellInfo {
                cache_key: glyph.cache_key,
                glyph_span: glyph.glyph_span,
                cell_index: u8::try_from(span_idx).unwrap_or(u8::MAX),
            },
        );
    }
}

/// Check whether `glyph` is a contextual alternate in `run`.
///
/// A glyph is a contextual alternate when its run-shaped glyph identity
/// (font + glyph ID) differs from the identity produced by shaping the
/// character in isolation.  We compare only `font_id` and `glyph_id`,
/// ignoring subpixel bins (`x_bin`, `y_bin`) which vary by position
/// within the run and would cause false positives.
///
/// The source character comes from `glyph.source_char` rather than indexing
/// into a `chars` vec by `col_offset`: `col_offset` counts wide characters
/// (e.g. emoji) as multiple grid columns, but `chars` indexes them as one
/// entry, so the two diverge after any wide character.
fn is_contextual_alternate(
    atlas: &mut GlyphAtlas,
    glyph: &ShapedRunGlyph,
    run: &StyledRun,
) -> bool {
    if glyph.source_char == '\0' {
        return false;
    }
    atlas
        .shape_single_cache_key(
            glyph.source_char,
            GlyphStyle { bold: run.bold, italic: run.italic },
        )
        .is_some_and(|solo_key| {
            solo_key.font_id != glyph.cache_key.font_id
                || solo_key.glyph_id != glyph.cache_key.glyph_id
        })
}

/// Build a map from `(line, column)` to [`LigatureCellInfo`] for every cell
/// that participates in a ligature or contextual alternate.
///
/// For each [`StyledRun`], the run is shaped as a whole.  Glyphs that span
/// more than one column (`glyph_span > 1`) are always recorded.  Single-cell
/// glyphs are only recorded when their shaped `CacheKey` differs from the
/// key produced by shaping the character in isolation.
///
/// Many monospace fonts (e.g. `JetBrains Mono`) implement ligatures using a
/// pattern of N-1 empty placeholder glyphs followed by one wide glyph with
/// large negative left bearing that visually covers all N cells.  This
/// function detects that pattern and merges the group into a single
/// multi-cell entry so the renderer can split the visual glyph across
/// the correct cells.
fn build_ligature_map(runs: &[StyledRun], atlas: &mut GlyphAtlas) -> LigatureMap {
    let mut map = LigatureMap::new();

    for run in runs {
        let shaped =
            atlas.shape_run(&run.text, GlyphStyle { bold: run.bold, italic: run.italic }).to_vec();

        let mut i = 0;
        while let Some(glyph) = shaped.get(i) {
            let col = run.start_col + glyph.col_offset;

            // Multi-cell glyph from the shaper — use as-is.
            if glyph.glyph_span > 1 {
                insert_multi_cell_glyph(&mut map, run.line, col, glyph);
                i += 1;
                continue;
            }

            if !is_contextual_alternate(atlas, glyph, run) {
                i += 1;
                continue;
            }

            // Contextual alternate that fits in a single cell — record it.
            if atlas.fits_single_cell(glyph.cache_key) {
                map.insert(
                    (run.line, col),
                    LigatureCellInfo { cache_key: glyph.cache_key, glyph_span: 1, cell_index: 0 },
                );
                i += 1;
                continue;
            }

            // This glyph doesn't fit a single cell (empty placeholder or
            // oversized).  Scan ahead to count consecutive such glyphs.
            let phantom_start = i;
            while shaped.get(i).is_some_and(|g| {
                g.glyph_span == 1
                    && is_contextual_alternate(atlas, g, run)
                    && !atlas.fits_single_cell(g.cache_key)
            }) {
                i += 1;
            }
            let phantom_count = i - phantom_start;

            // If the next glyph is a renderable contextual alternate
            // adjacent to the phantoms, merge them into one multi-cell
            // ligature using the visual glyph's cache key.
            let phantom_start_col = shaped.get(phantom_start).map_or(0, |g| g.col_offset);
            let visual = shaped.get(i).filter(|next| {
                next.glyph_span == 1
                    && next.col_offset == phantom_start_col + phantom_count
                    && is_contextual_alternate(atlas, next, run)
            });
            if let Some(visual) = visual {
                let total_span =
                    u8::try_from((phantom_count + 1).min(usize::from(u8::MAX))).unwrap_or(u8::MAX);
                let merged = ShapedRunGlyph {
                    cache_key: visual.cache_key,
                    col_offset: phantom_start_col,
                    glyph_span: total_span,
                    source_char: visual.source_char,
                };
                insert_multi_cell_glyph(&mut map, run.line, col, &merged);
                i += 1;
            }
            // else: orphan phantoms with no visual partner — skip them,
            // letting the cells fall through to regular per-character rendering.
        }
    }

    map
}
