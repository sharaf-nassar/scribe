use std::collections::HashMap;

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use wgpu::{
    Device, Extent3d, FilterMode, Origin3d, Queue, SamplerDescriptor, TexelCopyBufferLayout,
    TexelCopyTextureInfo, TextureAspect, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureViewDescriptor,
};

use crate::types::CellSize;

/// Atlas texture size (width = height).
const ATLAS_SIZE: u32 = 1024;

fn atlas_units_f32(units: u32) -> f32 {
    f32::from(u16::try_from(units).unwrap_or(u16::MAX))
}

fn atlas_signed_units_f32(units: i32) -> f32 {
    if units >= 0 {
        atlas_units_f32(u32::try_from(units).unwrap_or(u32::MAX))
    } else {
        -atlas_units_f32(units.unsigned_abs())
    }
}

fn atlas_nonnegative_i32_to_u32(units: i32) -> u32 {
    u32::try_from(units.max(0)).unwrap_or(u32::MAX)
}

fn atlas_ceil_u16(value: f32) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low) / 2;
        if f32::from(mid) < value {
            low = mid.saturating_add(1);
        } else {
            high = mid;
        }
    }
    low
}

fn atlas_ceil_u32(value: f32) -> u32 {
    u32::from(atlas_ceil_u16(value))
}

fn atlas_round_u8(value: f32) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    let target = value + 0.5;
    let mut low = 0u8;
    let mut high = u8::MAX;
    while low < high {
        let mid = low + (high - low) / 2;
        if f32::from(mid) < target {
            low = mid.saturating_add(1);
        } else {
            high = mid;
        }
    }
    low.saturating_sub(1)
}

fn atlas_buffer_len(width: u32, height: u32) -> usize {
    usize::try_from(width)
        .unwrap_or(usize::MAX)
        .saturating_mul(usize::try_from(height).unwrap_or(usize::MAX))
        .saturating_mul(4)
}

fn atlas_offset(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Key that uniquely identifies one rasterised glyph variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub c: char,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct GlyphStyle {
    pub bold: bool,
    pub italic: bool,
}

/// UV coordinates of a glyph within the atlas texture.
#[derive(Debug, Clone, Copy)]
pub struct GlyphEntry {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
}

/// Cache key for a shaped text run: the raw text plus font variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunShapeKey {
    text: String,
    style: GlyphStyle,
}

/// One glyph produced by shaping a multi-character run.
#[derive(Debug, Clone)]
pub struct ShapedRunGlyph {
    /// Swash cache key for this glyph (used with `get_or_insert_shaped`).
    pub cache_key: CacheKey,
    /// Column offset within the run (0-indexed). Counts wide characters
    /// (`glyph_span > 1`) as multiple columns, so it tracks grid position
    /// rather than character index.
    pub col_offset: usize,
    /// Number of terminal columns this glyph occupies.
    pub glyph_span: u8,
    /// The first character of the source bytes that produced this glyph.
    /// Used by callers (e.g. contextual-alternate detection) to look up the
    /// glyph's source character without indexing into a `chars` vec, which
    /// would diverge from `col_offset` after any wide character.
    pub source_char: char,
}

/// Bundled font configuration for atlas construction.
#[derive(Debug, Clone)]
pub struct FontParams {
    pub family: String,
    pub size: f32,
    pub weight: u16,
    pub weight_bold: u16,
    pub ligatures: bool,
    pub line_padding: u16,
}

/// Simple shelf-based rectangle packer.
struct ShelfPacker {
    cursor_x: u32,
    cursor_y: u32,
    shelf_height: u32,
    atlas_size: u32,
}

impl ShelfPacker {
    const fn new(atlas_size: u32) -> Self {
        // Start at (1,1) to reserve a transparent-black pixel at (0,0).
        // Empty cells use UV [0,0]->[0,0] which samples this region,
        // guaranteeing alpha=0 so mix(bg, fg, 0) = pure background.
        Self { cursor_x: 1, cursor_y: 1, shelf_height: 0, atlas_size }
    }

    /// Try to place a rectangle of `width` × `height`.
    ///
    /// Returns `Some((x, y))` on success, `None` if the atlas is full.
    fn pack(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width == 0 || height == 0 {
            return None;
        }

        // Advance to the next shelf if the glyph does not fit on the current row.
        if self.cursor_x + width > self.atlas_size {
            self.cursor_y = self.cursor_y.saturating_add(self.shelf_height);
            self.cursor_x = 0;
            self.shelf_height = 0;
        }

        // Check vertical overflow.
        if self.cursor_y + height > self.atlas_size {
            return None;
        }

        let x = self.cursor_x;
        let y = self.cursor_y;

        // 1px padding between entries prevents atlas bleeding under
        // bilinear filtering (adjacent glyph pixels blending into edges).
        self.cursor_x = self.cursor_x.saturating_add(width + 1);
        if height > self.shelf_height {
            self.shelf_height = height + 1;
        }

        Some((x, y))
    }
}

/// Glyph atlas: rasterises glyphs via cosmic-text and caches them in a wgpu
/// RGBA8 texture.
pub struct GlyphAtlas {
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    cache: HashMap<GlyphKey, GlyphEntry>,
    shaped_cache: HashMap<(CacheKey, u8), GlyphEntry>,
    run_shape_cache: HashMap<RunShapeKey, Vec<ShapedRunGlyph>>,
    packer: ShelfPacker,
    font_system: FontSystem,
    swash_cache: SwashCache,
    metrics: Metrics,
    cell_size: CellSize,
    atlas_size: u32,
    /// Owned family name; `None` means fall back to system monospace.
    family_name: Option<String>,
    font_weight: u16,
    font_weight_bold: u16,
    ligatures: bool,
}

impl GlyphAtlas {
    /// Create a new atlas with the given font parameters.
    pub fn new(device: &Device, queue: &Queue, params: &FontParams) -> Self {
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        // Validate the requested font family against fontdb; fall back to
        // the system monospace if the family is not found.
        let family_name = resolve_family(&font_system, params);

        // line_height = font_size * 1.2 plus any configured line padding.
        let line_height = params.size * 1.2 + f32::from(params.line_padding);
        let metrics = Metrics::new(params.size, line_height);

        // Measure the cell size by shaping "M" (a wide capital letter).
        let family = family_name_to_cosmic(family_name.as_deref());
        let cell_size = measure_cell(&mut font_system, metrics, family, params.ligatures);

        // Create the atlas texture.
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("glyph_atlas"),
            size: Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            // Alpha-coverage only; no sRGB colour data stored in this atlas.
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Clear the texture to transparent black on creation.
        clear_texture(queue, &texture, ATLAS_SIZE);

        let texture_view = texture.create_view(&TextureViewDescriptor::default());

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("glyph_atlas_sampler"),
            mag_filter: FilterMode::Nearest,
            min_filter: FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            texture,
            texture_view,
            sampler,
            cache: HashMap::new(),
            shaped_cache: HashMap::new(),
            run_shape_cache: HashMap::new(),
            packer: ShelfPacker::new(ATLAS_SIZE),
            font_system,
            swash_cache,
            metrics,
            cell_size,
            atlas_size: ATLAS_SIZE,
            family_name,
            font_weight: params.weight,
            font_weight_bold: params.weight_bold,
            ligatures: params.ligatures,
        }
    }

    /// Return the cell size measured from the font.
    pub const fn cell_size(&self) -> CellSize {
        self.cell_size
    }

    /// Whether ligature shaping is enabled.
    pub const fn ligatures(&self) -> bool {
        self.ligatures
    }

    /// Return the atlas texture view (used for bind groups).
    pub const fn texture_view(&self) -> &wgpu::TextureView {
        &self.texture_view
    }

    /// Return the atlas sampler (used for bind groups).
    pub const fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// Look up a glyph entry in the cache; rasterise and upload on a miss.
    ///
    /// Returns a fallback entry (zeroed UVs) if the glyph cannot be packed.
    /// Atlas-overflow failures are NOT cached so a future rebuild can retry.
    pub fn get_or_insert(&mut self, device: &Device, queue: &Queue, key: GlyphKey) -> GlyphEntry {
        if let Some(entry) = self.cache.get(&key) {
            return *entry;
        }

        // Rasterise on a cache miss.
        let entry = self.rasterize(queue, key);
        let _ = device; // device reserved for future atlas resize
        // Only cache entries with valid UVs; a zero-UV result from atlas
        // overflow must not be stored so a future rebuild can try again.
        if entry.uv_max != [0.0, 0.0] {
            self.cache.insert(key, entry);
        }
        entry
    }

    /// Rasterise a single glyph and upload it to the atlas texture.
    fn rasterize(&mut self, queue: &Queue, key: GlyphKey) -> GlyphEntry {
        let Some((width, height, rgba)) = self.rasterize_rgba(key) else {
            return Self::empty_entry();
        };

        let Some((px, py)) = self.packer.pack(width, height) else {
            tracing::warn!(
                c = %key.c,
                bold = key.bold,
                italic = key.italic,
                "glyph atlas full — could not pack glyph; rebuild atlas to recover"
            );
            return Self::empty_entry();
        };

        upload_glyph(queue, &self.texture, &UploadParams { px, py, width, height, rgba: &rgba });
        // For normal glyphs the canvas is ceil(cell_w) × ceil(cell_h).
        // Use the float cell dimensions for the UV so the shader's
        // cell-sized quad maps 1:1 to texels (no Nearest-filter skipping).
        //
        // For overflow glyphs (canvas wider than ceil(cell_w), e.g. ⚙),
        // keep the full canvas width so the shader scales the whole glyph
        // into the cell quad.
        let cell_w_ceil = self.cell_size.width.ceil();
        let width_f = atlas_units_f32(width);
        let uv_w = if width_f > cell_w_ceil { width_f } else { self.cell_size.width };
        compute_uvs(px, py, uv_w, self.cell_size.height, self.atlas_size)
    }

    /// Shape the character, rasterise it into a cell-sized RGBA canvas.
    ///
    /// Every glyph is composited onto a `cell_width x cell_height` buffer
    /// using the swash placement offsets, so the UV maps 1:1 to the cell
    /// quad with no stretching or misalignment.
    ///
    /// Returns `Some((cell_w, cell_h, rgba))` or `None` if the glyph is
    /// empty or uses an unsupported pixel format.
    fn rasterize_rgba(&mut self, key: GlyphKey) -> Option<(u32, u32, Vec<u8>)> {
        // Box-drawing and block elements are rendered procedurally so they
        // fill the cell edge-to-edge with no font-bearing gaps.
        if crate::box_drawing::is_box_drawing(key.c) {
            let cell_w = atlas_ceil_u32(self.cell_size.width);
            let cell_h = atlas_ceil_u32(self.cell_size.height);
            if let Some(result) = crate::box_drawing::render(key.c, cell_w, cell_h) {
                return Some(result);
            }
            // Fall through to font rasterisation for unhandled variants
            // (e.g. diagonal lines ╱╲╳).
        }

        let cache_key = self.shape_cache_key(key)?;

        let image_parts =
            self.swash_cache.get_image(&mut self.font_system, cache_key).as_ref().map(|img| {
                (
                    img.placement.width,
                    img.placement.height,
                    img.placement.left,
                    img.placement.top,
                    img.content,
                    img.data.clone(),
                )
            });

        let (glyph_w, glyph_h, left, top, content, data) = image_parts?;
        if glyph_w == 0 || glyph_h == 0 {
            return None;
        }

        let glyph_rgba = content_to_rgba(content, data)?;

        // Cell dimensions (the atlas entry will be exactly this size).
        let cell_w = atlas_ceil_u32(self.cell_size.width);
        let cell_h = atlas_ceil_u32(self.cell_size.height);
        if cell_w == 0 || cell_h == 0 {
            return None;
        }

        // Destination offset within the cell canvas:
        //   x: placement.left (horizontal bearing from cell origin)
        //   y: font_size acts as approximate ascent; top is distance above baseline
        let dest_x = atlas_nonnegative_i32_to_u32(left);
        let dest_y =
            atlas_ceil_u32((self.metrics.font_size - atlas_signed_units_f32(top)).max(0.0));

        // Canvas width: expand beyond cell_w if the glyph overflows
        // horizontally (e.g. ⚙ U+2699 is wider than one monospace cell
        // in many fonts). The atlas stores the full glyph and the shader
        // maps its UV onto the cell-sized quad, scaling it to fit.
        let canvas_w = cell_w.max(dest_x.saturating_add(glyph_w));
        let mut canvas = vec![0u8; atlas_buffer_len(canvas_w, cell_h)];

        // Blit glyph pixels onto the canvas.
        blit_glyph(
            &glyph_rgba,
            &mut canvas,
            &BlitParams {
                src_w: glyph_w,
                src_h: glyph_h,
                dst_w: canvas_w,
                dst_h: cell_h,
                dest_x,
                dest_y,
            },
        );

        Some((canvas_w, cell_h, canvas))
    }

    /// Shape the character, rasterise it into a multi-cell RGBA canvas.
    ///
    /// Like `rasterize_rgba` but accepts a `CacheKey` directly and supports a
    /// canvas that is `glyph_span` cells wide (for ligature glyphs that span
    /// multiple columns).
    ///
    /// Returns `Some((canvas_w, canvas_h, rgba))` or `None` if the glyph is
    /// empty or uses an unsupported pixel format.
    fn rasterize_from_cache_key(
        &mut self,
        cache_key: CacheKey,
        glyph_span: u8,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let image_parts =
            self.swash_cache.get_image(&mut self.font_system, cache_key).as_ref().map(|img| {
                (
                    img.placement.width,
                    img.placement.height,
                    img.placement.left,
                    img.placement.top,
                    img.content,
                    img.data.clone(),
                )
            });

        let (glyph_w, glyph_h, left, top, content, data) = image_parts?;
        if glyph_w == 0 || glyph_h == 0 {
            return None;
        }

        let glyph_rgba = content_to_rgba(content, data)?;

        let cell_w = atlas_ceil_u32(self.cell_size.width);
        let cell_h = atlas_ceil_u32(self.cell_size.height);
        if cell_w == 0 || cell_h == 0 {
            return None;
        }

        // Canvas is `glyph_span` cells wide to accommodate multi-col glyphs.
        let canvas_w = cell_w.saturating_mul(u32::from(glyph_span));
        let mut canvas = vec![0u8; atlas_buffer_len(canvas_w, cell_h)];

        // For multi-cell glyphs with negative left bearing, the glyph's
        // origin is not at the left edge of the canvas.  Monospace fonts
        // like JetBrains Mono use empty placeholder glyphs for the leading
        // cells and place all visual content in the last glyph, which
        // extends backward via negative bearing.  Compute the origin cell
        // from the bearing magnitude so the glyph lands in the right place.
        let dest_x = if left < 0 && glyph_span > 1 {
            let cell_w_f = self.cell_size.width;
            let cells_before = atlas_ceil_u32(atlas_units_f32(left.unsigned_abs()) / cell_w_f);
            let origin_x = cells_before.min(u32::from(glyph_span) - 1) * cell_w;
            atlas_nonnegative_i32_to_u32(
                i32::try_from(origin_x).unwrap_or(i32::MAX).saturating_add(left),
            )
        } else {
            atlas_nonnegative_i32_to_u32(left)
        };
        let dest_y =
            atlas_ceil_u32((self.metrics.font_size - atlas_signed_units_f32(top)).max(0.0));

        blit_glyph(
            &glyph_rgba,
            &mut canvas,
            &BlitParams {
                src_w: glyph_w,
                src_h: glyph_h,
                dst_w: canvas_w,
                dst_h: cell_h,
                dest_x,
                dest_y,
            },
        );

        Some((canvas_w, cell_h, canvas))
    }

    /// Look up a shaped glyph entry by its raw `CacheKey`; rasterise and
    /// upload to the atlas on a miss.
    ///
    /// `glyph_span` is the number of terminal columns the glyph occupies
    /// (1 for normal glyphs, >1 for ligature / wide glyphs).
    ///
    /// Returns a fallback entry (zeroed UVs) if the glyph cannot be packed.
    pub fn get_or_insert_shaped(
        &mut self,
        queue: &Queue,
        cache_key: CacheKey,
        glyph_span: u8,
    ) -> GlyphEntry {
        // Cap the shaped glyph cache to avoid unbounded growth from many unique
        // glyphs (e.g. unicode-heavy output over long sessions).
        if self.shaped_cache.len() > 8192 {
            // Evict roughly half the entries instead of clearing the entire cache.
            // This avoids a burst of cache misses after eviction.
            let mut keep = false;
            self.shaped_cache.retain(|_, _| {
                keep = !keep;
                keep
            });
        }
        let key = (cache_key, glyph_span);
        if let Some(entry) = self.shaped_cache.get(&key) {
            return *entry;
        }

        let entry = self.rasterize_shaped(queue, cache_key, glyph_span);
        // Only cache entries with valid UVs; a zero-UV result from atlas
        // overflow must not be stored so a future rebuild can try again.
        if entry.uv_max != [0.0, 0.0] {
            self.shaped_cache.insert(key, entry);
        }
        entry
    }

    /// Rasterise a shaped glyph by `CacheKey` and upload it to the atlas.
    fn rasterize_shaped(
        &mut self,
        queue: &Queue,
        cache_key: CacheKey,
        glyph_span: u8,
    ) -> GlyphEntry {
        let Some((width, height, rgba)) = self.rasterize_from_cache_key(cache_key, glyph_span)
        else {
            return Self::empty_entry();
        };

        let Some((px, py)) = self.packer.pack(width, height) else {
            tracing::warn!(
                glyph_span,
                "glyph atlas full — could not pack shaped glyph; rebuild atlas to recover"
            );
            return Self::empty_entry();
        };

        upload_glyph(queue, &self.texture, &UploadParams { px, py, width, height, rgba: &rgba });
        // UV spans the float cell dimensions × glyph_span, matching the
        // GPU quad size for this multi-cell glyph.
        let uv_w = self.cell_size.width * f32::from(glyph_span);
        compute_uvs(px, py, uv_w, self.cell_size.height, self.atlas_size)
    }

    /// Shape a multi-character text run and return the resulting glyph list.
    ///
    /// Results are cached by `(text, bold, italic)`. The returned slice is
    /// valid for the lifetime of the atlas.
    ///
    /// Uses a two-step cache lookup (`contains_key` then `get`) to avoid
    /// holding a borrow across the mutable call to `shape_run_uncached`.
    /// The key is constructed once and reused for both the miss-insert and the
    /// final lookup, avoiding a second `to_owned()` allocation.
    pub fn shape_run(&mut self, text: &str, style: GlyphStyle) -> &[ShapedRunGlyph] {
        // Cap the run shape cache to avoid unbounded growth across many unique
        // text runs (e.g. long-running sessions with varied output).
        if self.run_shape_cache.len() > 4096 {
            // Evict roughly half the entries instead of clearing the entire cache.
            // This avoids a burst of cache misses after eviction.
            let mut keep = false;
            self.run_shape_cache.retain(|_, _| {
                keep = !keep;
                keep
            });
        }
        let key = RunShapeKey { text: text.to_owned(), style };
        if !self.run_shape_cache.contains_key(&key) {
            let glyphs = self.shape_run_uncached(text, style);
            // Move `key` into insert to avoid cloning the String.  The final
            // get below rebuilds a key, but that only runs on the cold miss path.
            self.run_shape_cache.insert(key, glyphs);
            let miss_key = RunShapeKey { text: text.to_owned(), style };
            if let Some(cached_glyphs) = self.run_shape_cache.get(&miss_key) {
                return cached_glyphs.as_slice();
            }
            return &[];
        }
        // The key was either already present or just inserted above; `get`
        // returns `None` only if the key is absent, which cannot happen here.
        // An empty-slice fallback is used instead of `unwrap` to satisfy the
        // `unwrap_used` lint — the fallback is unreachable in practice.
        self.run_shape_cache.get(&key).map_or(&[], Vec::as_slice)
    }

    /// Shape a text run without consulting the cache.
    ///
    /// Always uses `Shaping::Advanced` to enable ligature substitution.
    fn shape_run_uncached(&mut self, text: &str, style: GlyphStyle) -> Vec<ShapedRunGlyph> {
        let mut buf = Buffer::new_empty(self.metrics);
        let family_str = self.family_name.as_deref();
        let attrs =
            Self::build_attrs_from(family_str, self.font_weight, self.font_weight_bold, style);
        buf.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);

        let cell_w = self.cell_size.width;
        let mut glyphs = Vec::new();
        // Track column position incrementally instead of dividing g.x by cell_w.
        // The division-based approach accumulates floating-point drift across long
        // runs — by column 50+, multiple glyphs round to the same index.
        let mut next_col: usize = 0;

        for run in buf.layout_runs() {
            for g in run.glyphs {
                let cache_key = g.physical((0.0, 0.0), 1.0).cache_key;
                let glyph_span = atlas_round_u8((g.w / cell_w).round()).max(1);
                let col_offset = next_col;
                next_col += usize::from(glyph_span);
                let source_char =
                    text.get(g.start..g.end).and_then(|s| s.chars().next()).unwrap_or('\0');
                glyphs.push(ShapedRunGlyph { cache_key, col_offset, glyph_span, source_char });
            }
        }

        glyphs
    }

    /// Build `Attrs` from a family name string slice and raw bold/italic flags.
    ///
    /// This is a free helper (not `&self`) so that callers can extract the
    /// family name first and then freely borrow `self.font_system` mutably.
    fn build_attrs_from(
        family_name: Option<&str>,
        weight: u16,
        weight_bold: u16,
        style: GlyphStyle,
    ) -> Attrs<'_> {
        use cosmic_text::{Style, Weight};
        Attrs::new()
            .family(family_name_to_cosmic(family_name))
            .weight(Weight(if style.bold { weight_bold } else { weight }))
            .style(if style.italic { Style::Italic } else { Style::Normal })
    }

    /// Shape a single character and return its `CacheKey`.
    ///
    /// Useful for callers that need to compare single-glyph cache keys against
    /// the keys produced by `shape_run`.
    pub fn shape_single_cache_key(&mut self, c: char, style: GlyphStyle) -> Option<CacheKey> {
        self.shape_cache_key(GlyphKey { c, bold: style.bold, italic: style.italic })
    }

    /// Shape the character and return the first glyph's `CacheKey`.
    fn shape_cache_key(&mut self, key: GlyphKey) -> Option<cosmic_text::CacheKey> {
        let mut buf = Buffer::new_empty(self.metrics);
        let family_str = self.family_name.as_deref();
        let attrs = Self::build_attrs_from(
            family_str,
            self.font_weight,
            self.font_weight_bold,
            GlyphStyle { bold: key.bold, italic: key.italic },
        );
        let mut char_buf = [0u8; 4];
        let text = key.c.encode_utf8(&mut char_buf);
        let shaping = if self.ligatures { Shaping::Advanced } else { Shaping::Basic };
        buf.set_text(&mut self.font_system, text, &attrs, shaping, None);

        buf.layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.physical((0.0, 0.0), 1.0).cache_key)
    }

    /// Return an empty (zero-size) glyph entry used as a safe fallback.
    const fn empty_entry() -> GlyphEntry {
        GlyphEntry { uv_min: [0.0, 0.0], uv_max: [0.0, 0.0] }
    }

    /// Check if a shaped glyph produces visible content that fits in one cell.
    ///
    /// Returns `false` for empty placeholders (0×0 image) and for glyphs
    /// whose visual bounds extend well beyond a single cell.  Monospace fonts
    /// like `JetBrains Mono` use this pattern for ligatures: N-1 empty
    /// placeholder glyphs followed by one wide glyph with large negative
    /// left bearing.  Both the placeholders and the wide glyph fail this
    /// check, allowing [`build_ligature_map`] to merge them into a proper
    /// multi-cell ligature.
    pub fn fits_single_cell(&mut self, cache_key: CacheKey) -> bool {
        let image = self.swash_cache.get_image(&mut self.font_system, cache_key);
        let Some(img) = image.as_ref() else { return false };

        if img.placement.width == 0 || img.placement.height == 0 {
            return false;
        }

        let cell_w = i32::from(atlas_ceil_u16(self.cell_size.width));
        let max_left_extension = cell_w / 3;
        if img.placement.left < -max_left_extension {
            return false;
        }
        if i32::try_from(img.placement.width).unwrap_or(i32::MAX) > cell_w + cell_w / 2 {
            return false;
        }

        true
    }
}

/// Validate the font family name against the fontdb.
///
/// Returns `Some(name)` if the font is found, `None` to fall back to the
/// system monospace.  The caller stores the `String` and borrows from it
/// via [`family_name_to_cosmic`] — no heap leak needed.
fn resolve_family(font_system: &FontSystem, params: &FontParams) -> Option<String> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(&params.family)],
        weight: fontdb::Weight(params.weight),
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };

    if font_system.db().query(&query).is_some() {
        Some(params.family.clone())
    } else {
        tracing::warn!(family = %params.family, "font family not found, falling back to system monospace");
        None
    }
}

/// Convert an optional owned family name to a borrowed `cosmic_text::Family`.
fn family_name_to_cosmic(name: Option<&str>) -> Family<'_> {
    name.map_or(Family::Monospace, Family::Name)
}

/// Convert swash image content to a flat RGBA byte vector.
///
/// Returns `None` for `SubpixelMask` which is not currently supported.
fn content_to_rgba(content: SwashContent, data: Vec<u8>) -> Option<Vec<u8>> {
    match content {
        SwashContent::Mask => {
            // Single-byte alpha mask — expand to white RGBA.
            let mut out = Vec::with_capacity(data.len() * 4);
            for alpha in &data {
                out.extend_from_slice(&[255, 255, 255, *alpha]);
            }
            Some(out)
        }
        SwashContent::Color => Some(data),
        SwashContent::SubpixelMask => None,
    }
}

/// Source and destination geometry for [`blit_glyph`].
struct BlitParams {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    dest_x: u32,
    dest_y: u32,
}

/// Copy glyph RGBA pixels onto a cell-sized canvas, clipping any pixels
/// that fall outside the canvas bounds.
fn blit_glyph(src: &[u8], dst: &mut [u8], p: &BlitParams) {
    // Number of source columns that actually fit within the destination.
    let visible_w = p.src_w.min(p.dst_w.saturating_sub(p.dest_x));
    if visible_w == 0 {
        return;
    }
    let row_bytes = atlas_offset(visible_w).saturating_mul(4);

    for gy in 0..p.src_h {
        let cy = p.dest_y + gy;
        if cy >= p.dst_h {
            break;
        }
        let si = atlas_offset(gy.saturating_mul(p.src_w)).saturating_mul(4);
        let di =
            atlas_offset(cy.saturating_mul(p.dst_w).saturating_add(p.dest_x)).saturating_mul(4);
        if let (Some(s), Some(d)) = (src.get(si..si + row_bytes), dst.get_mut(di..di + row_bytes)) {
            d.copy_from_slice(s);
        }
    }
}

/// Parameters for uploading a glyph rectangle to the atlas texture.
struct UploadParams<'a> {
    px: u32,
    py: u32,
    width: u32,
    height: u32,
    rgba: &'a [u8],
}

/// Upload RGBA glyph data to the atlas texture at position `(px, py)`.
fn upload_glyph(queue: &Queue, texture: &wgpu::Texture, params: &UploadParams<'_>) {
    queue.write_texture(
        TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d { x: params.px, y: params.py, z: 0 },
            aspect: TextureAspect::All,
        },
        params.rgba,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(params.width * 4),
            rows_per_image: Some(params.height),
        },
        Extent3d { width: params.width, height: params.height, depth_or_array_layers: 1 },
    );
}

/// Compute normalised UV coordinates for a packed glyph.
///
/// `uv_width` / `uv_height` are the **float** cell dimensions (matching the
/// GPU quad size), not the ceil'd canvas dimensions.  This ensures the UV
/// window covers exactly the same number of texels as the shader quad has
/// pixels, preventing texel skipping under Nearest-filter sampling.
///
/// Atlas coordinates fit comfortably within f32 precision (max 1023 < 2^23).
fn compute_uvs(px: u32, py: u32, uv_width: f32, uv_height: f32, atlas_size: u32) -> GlyphEntry {
    let s = atlas_units_f32(atlas_size);
    GlyphEntry {
        uv_min: [atlas_units_f32(px) / s, atlas_units_f32(py) / s],
        uv_max: [(atlas_units_f32(px) + uv_width) / s, (atlas_units_f32(py) + uv_height) / s],
    }
}

/// Measure cell dimensions by shaping the capital letter "M".
fn measure_cell(
    font_system: &mut FontSystem,
    metrics: Metrics,
    family: Family<'_>,
    ligatures: bool,
) -> CellSize {
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(family);
    let shaping = if ligatures { Shaping::Advanced } else { Shaping::Basic };
    buf.set_text(font_system, "M", &attrs, shaping, None);

    // Advance width from the first glyph of the first layout run.
    let advance = buf
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first())
        .map_or(metrics.font_size, |g| g.w);

    CellSize { width: advance, height: metrics.line_height }
}

/// Fill the atlas texture with transparent black so it is well-defined.
///
/// This allocates a ~4 MB zeroed buffer (1024×1024×4 bytes) on construction.
/// It only runs once per atlas lifetime so the allocation cost is acceptable.
/// wgpu's `Features::CLEAR_TEXTURE` would avoid the CPU buffer, but requiring
/// that feature would break compatibility with some older backends.
fn clear_texture(queue: &Queue, texture: &wgpu::Texture, size: u32) {
    let pixel_count = atlas_offset(size).saturating_mul(atlas_offset(size));
    let data = vec![0u8; pixel_count * 4];
    queue.write_texture(
        TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        &data,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size * 4),
            rows_per_image: Some(size),
        },
        Extent3d { width: size, height: size, depth_or_array_layers: 1 },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `fits_single_cell` correctly rejects empty placeholder
    /// glyphs and oversized glyphs used in monospace ligature patterns.
    #[test]
    fn fits_single_cell_rejects_ligature_placeholders() {
        let mut font_system = FontSystem::new();
        let size = 16.0;
        let line_height = size * 1.2;
        let metrics = Metrics::new(size, line_height);
        let family = Family::Name("JetBrains Mono");
        let attrs = Attrs::new().family(family);

        // Measure cell width.
        let cell_w = {
            let mut buf = Buffer::new_empty(metrics);
            buf.set_text(&mut font_system, "M", &attrs, Shaping::Advanced, None);
            buf.layout_runs().next().and_then(|run| run.glyphs.first()).map_or(size, |g| g.w)
        };
        let cell_size = CellSize { width: cell_w, height: line_height };
        let mut swash = SwashCache::new();

        // Shape "//" with ligatures → two contextual alternates.
        let mut buf = Buffer::new_empty(metrics);
        buf.set_text(&mut font_system, "//", &attrs, Shaping::Advanced, None);
        let glyphs: Vec<_> = buf
            .layout_runs()
            .flat_map(|r| r.glyphs.iter())
            .map(|g| g.physical((0.0, 0.0), 1.0).cache_key)
            .collect();
        assert_eq!(glyphs.len(), 2, "expected 2 shaped glyphs for '//'");

        // First glyph (empty placeholder) must NOT fit a single cell.
        let img0 = swash.get_image(&mut font_system, glyphs[0]);
        let empty = img0.as_ref().is_none_or(|i| i.placement.width == 0 || i.placement.height == 0);
        assert!(empty, "first glyph of '//' ligature should be empty");

        // Build a minimal atlas just for the fits_single_cell check.
        // We only need font_system, swash_cache, and cell_size.
        let mut mini = MiniFitChecker { font_system, swash_cache: swash, cell_size };
        assert!(!mini.fits(glyphs[0]), "empty placeholder must not fit single cell");
        assert!(!mini.fits(glyphs[1]), "wide ligature glyph must not fit single cell");

        // Solo "/" should fit.
        let mut buf_solo = Buffer::new_empty(metrics);
        buf_solo.set_text(&mut mini.font_system, "/", &attrs, Shaping::Advanced, None);
        let solo_key = buf_solo
            .layout_runs()
            .next()
            .and_then(|r| r.glyphs.first())
            .map(|g| g.physical((0.0, 0.0), 1.0).cache_key)
            .expect("solo '/' should produce a glyph");
        assert!(mini.fits(solo_key), "solo '/' must fit single cell");
    }

    /// Helper to run the same logic as `GlyphAtlas::fits_single_cell`
    /// without needing a GPU device.
    struct MiniFitChecker {
        font_system: FontSystem,
        swash_cache: SwashCache,
        cell_size: CellSize,
    }

    impl MiniFitChecker {
        fn fits(&mut self, cache_key: CacheKey) -> bool {
            let image = self.swash_cache.get_image(&mut self.font_system, cache_key);
            let Some(img) = image.as_ref() else { return false };
            if img.placement.width == 0 || img.placement.height == 0 {
                return false;
            }
            let cell_w = i32::from(atlas_ceil_u16(self.cell_size.width));
            let max_ext = cell_w / 3;
            if img.placement.left < -max_ext {
                return false;
            }
            if i32::try_from(img.placement.width).unwrap_or(i32::MAX) > cell_w + cell_w / 2 {
                return false;
            }
            true
        }
    }

    /// Regression test for the wide-character contextual-alternate bug: after
    /// a wide character (e.g. emoji) the cumulative `col_offset` no longer
    /// matches the source character index, so identifying the source character
    /// by indexing a `chars` vec would return the wrong character. The
    /// `source_char` field on `ShapedRunGlyph` must be populated from the
    /// glyph's source byte range so identity checks (contextual-alternate
    /// detection) get the right character regardless of column offset.
    #[test]
    fn shape_run_records_source_char_for_each_glyph() {
        let mut font_system = FontSystem::new();
        let metrics = Metrics::new(16.0, 19.2);
        let family = Family::Name("JetBrains Mono");
        let attrs = Attrs::new().family(family);

        // Measure cell width.
        let cell_w = {
            let mut buf = Buffer::new_empty(metrics);
            buf.set_text(&mut font_system, "M", &attrs, Shaping::Advanced, None);
            buf.layout_runs().next().and_then(|r| r.glyphs.first()).map_or(16.0, |g| g.w)
        };

        // Shape the text using the same logic as `shape_run_uncached`. We
        // can't construct a full `GlyphAtlas` without a wgpu device, so we
        // reproduce the column-offset and source-char extraction inline.
        let text = "## 🌟 Bonus";
        let chars: Vec<char> = text.chars().collect();
        // chars: ['#', '#', ' ', '🌟', ' ', 'B', 'o', 'n', 'u', 's'] (10 entries).

        let mut buf = Buffer::new_empty(metrics);
        buf.set_text(&mut font_system, text, &attrs, Shaping::Advanced, None);

        let mut shaped: Vec<(char, usize, u8)> = Vec::new();
        let mut next_col: usize = 0;
        for run in buf.layout_runs() {
            for g in run.glyphs {
                let glyph_span = atlas_round_u8((g.w / cell_w).round()).max(1);
                let col_offset = next_col;
                next_col += usize::from(glyph_span);
                let source_char =
                    text.get(g.start..g.end).and_then(|s| s.chars().next()).unwrap_or('\0');
                shaped.push((source_char, col_offset, glyph_span));
            }
        }

        // Locate the 'B' glyph and verify the col_offset / chars-index split.
        let b_idx = shaped
            .iter()
            .position(|(c, _, _)| *c == 'B')
            .expect("'B' must appear among shaped glyphs");
        let (b_char, b_col_offset, b_span) = shaped[b_idx];
        assert_eq!(b_char, 'B');
        assert_eq!(b_span, 1);
        // 'B' is at character index 5 in the source text but at grid column
        // offset 6 — the wide '🌟' takes columns 3 and 4.
        assert_eq!(chars.iter().position(|c| *c == 'B'), Some(5));
        assert_eq!(b_col_offset, 6);
        // This is the bug we fixed: indexing `chars` by `col_offset` would
        // return the wrong character ('o' at index 6, not 'B').
        assert_eq!(
            chars[b_col_offset], 'o',
            "regression guard: chars[col_offset] returns the wrong character after a wide char",
        );
        assert_ne!(chars[b_col_offset], b_char);

        // Sanity-check the emoji entry: source_char is the actual emoji and
        // glyph_span is 2 (so it consumes columns 3 and 4).
        let emoji = shaped.iter().find(|(c, _, _)| *c == '🌟').expect("emoji glyph present");
        assert_eq!(emoji.1, 3, "emoji starts at grid column 3");
        assert_eq!(emoji.2, 2, "emoji spans 2 grid columns");
    }
}
