use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use wgpu::{
    Device, Extent3d, FilterMode, Origin3d, Queue, SamplerDescriptor, TexelCopyBufferLayout,
    TexelCopyTextureInfo, TextureAspect, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureViewDescriptor,
};

use crate::types::CellSize;

/// Atlas texture size (width = height).
const ATLAS_SIZE: u32 = 1024;

/// Key that uniquely identifies one rasterised glyph variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub c: char,
    pub bold: bool,
    pub italic: bool,
}

/// UV coordinates of a glyph within the atlas texture.
#[derive(Debug, Clone, Copy)]
pub struct GlyphEntry {
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
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

        self.cursor_x = self.cursor_x.saturating_add(width);
        if height > self.shelf_height {
            self.shelf_height = height;
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
    packer: ShelfPacker,
    font_system: FontSystem,
    swash_cache: SwashCache,
    metrics: Metrics,
    cell_size: CellSize,
    atlas_size: u32,
}

impl GlyphAtlas {
    /// Create a new atlas.
    ///
    /// `font_size` controls the pixel size of the font used for all glyphs.
    pub fn new(device: &Device, queue: &Queue, font_size: f32) -> Self {
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        // Use line_height = font_size * 1.2 as a sensible default.
        let line_height = font_size * 1.2;
        let metrics = Metrics::new(font_size, line_height);

        // Measure the cell size by shaping "M" (a wide capital letter).
        let cell_size = measure_cell(&mut font_system, metrics);

        // Create the atlas texture.
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("glyph_atlas"),
            size: Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Clear the texture to transparent black on creation.
        clear_texture(queue, &texture, ATLAS_SIZE);

        let texture_view = texture.create_view(&TextureViewDescriptor::default());

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("glyph_atlas_sampler"),
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        Self {
            texture,
            texture_view,
            sampler,
            cache: HashMap::new(),
            packer: ShelfPacker::new(ATLAS_SIZE),
            font_system,
            swash_cache,
            metrics,
            cell_size,
            atlas_size: ATLAS_SIZE,
        }
    }

    /// Return the cell size measured from the font.
    pub const fn cell_size(&self) -> CellSize {
        self.cell_size
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
    pub fn get_or_insert(&mut self, device: &Device, queue: &Queue, key: GlyphKey) -> GlyphEntry {
        if let Some(entry) = self.cache.get(&key) {
            return *entry;
        }

        // Rasterise on a cache miss.
        let entry = self.rasterize(queue, key);
        let _ = device; // device reserved for future atlas resize
        self.cache.insert(key, entry);
        entry
    }

    /// Rasterise a single glyph and upload it to the atlas texture.
    fn rasterize(&mut self, queue: &Queue, key: GlyphKey) -> GlyphEntry {
        let Some((width, height, rgba)) = self.rasterize_rgba(key) else {
            return Self::empty_entry();
        };

        let Some((px, py)) = self.packer.pack(width, height) else {
            return Self::empty_entry();
        };

        upload_glyph(queue, &self.texture, &UploadParams { px, py, width, height, rgba: &rgba });
        compute_uvs(px, py, width, height, self.atlas_size)
    }

    /// Shape the character, rasterise it into a cell-sized RGBA canvas.
    ///
    /// Every glyph is composited onto a `cell_width x cell_height` buffer
    /// using the swash placement offsets, so the UV maps 1:1 to the cell
    /// quad with no stretching or misalignment.
    ///
    /// Returns `Some((cell_w, cell_h, rgba))` or `None` if the glyph is
    /// empty or uses an unsupported pixel format.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "cell dimensions and glyph offsets are small values that fit in u32"
    )]
    #[allow(
        clippy::cast_sign_loss,
        reason = "placement offsets are clamped to non-negative before cast"
    )]
    fn rasterize_rgba(&mut self, key: GlyphKey) -> Option<(u32, u32, Vec<u8>)> {
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
        let cell_w = self.cell_size.width.ceil() as u32;
        let cell_h = self.cell_size.height.ceil() as u32;
        if cell_w == 0 || cell_h == 0 {
            return None;
        }

        let mut canvas = vec![0u8; (cell_w * cell_h * 4) as usize];

        // Destination offset within the cell canvas:
        //   x: placement.left (horizontal bearing from cell origin)
        //   y: font_size acts as approximate ascent; top is distance above baseline
        let dest_x = left.max(0) as u32;
        #[allow(
            clippy::cast_precision_loss,
            reason = "placement.top is a small integer that fits exactly in f32"
        )]
        let dest_y = (self.metrics.font_size - top as f32).max(0.0) as u32;

        // Blit glyph pixels onto the canvas, clipping to cell bounds.
        blit_glyph(
            &glyph_rgba,
            &mut canvas,
            &BlitParams {
                src_w: glyph_w,
                src_h: glyph_h,
                dst_w: cell_w,
                dst_h: cell_h,
                dest_x,
                dest_y,
            },
        );

        Some((cell_w, cell_h, canvas))
    }

    /// Shape the character and return the first glyph's `CacheKey`.
    fn shape_cache_key(&mut self, key: GlyphKey) -> Option<cosmic_text::CacheKey> {
        let mut buf = Buffer::new_empty(self.metrics);
        let attrs = build_attrs(key);
        let mut char_buf = [0u8; 4];
        let text = key.c.encode_utf8(&mut char_buf);
        buf.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);

        buf.layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.physical((0.0, 0.0), 1.0).cache_key)
    }

    /// Return an empty (zero-size) glyph entry used as a safe fallback.
    const fn empty_entry() -> GlyphEntry {
        GlyphEntry { uv_min: [0.0, 0.0], uv_max: [0.0, 0.0] }
    }
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
    for gy in 0..p.src_h {
        let cy = p.dest_y + gy;
        if cy >= p.dst_h {
            break;
        }
        for gx in 0..p.src_w {
            let cx = p.dest_x + gx;
            if cx >= p.dst_w {
                continue;
            }
            let si = (gy * p.src_w + gx) as usize * 4;
            let di = (cy * p.dst_w + cx) as usize * 4;
            if let (Some(s), Some(d)) = (src.get(si..si + 4), dst.get_mut(di..di + 4)) {
                d.copy_from_slice(s);
            }
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
/// Atlas coordinates fit comfortably within f32 precision (max 1023 < 2^23).
#[allow(
    clippy::cast_precision_loss,
    reason = "atlas coordinates ≤ 1023 fit exactly in f32 mantissa"
)]
fn compute_uvs(px: u32, py: u32, width: u32, height: u32, atlas_size: u32) -> GlyphEntry {
    let s = atlas_size as f32;
    GlyphEntry {
        uv_min: [px as f32 / s, py as f32 / s],
        uv_max: [(px + width) as f32 / s, (py + height) as f32 / s],
    }
}

/// Build `Attrs` for the given `GlyphKey`.
fn build_attrs(key: GlyphKey) -> Attrs<'static> {
    use cosmic_text::{Style, Weight};
    Attrs::new()
        .family(Family::Monospace)
        .weight(if key.bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if key.italic { Style::Italic } else { Style::Normal })
}

/// Measure cell dimensions by shaping the capital letter "M".
fn measure_cell(font_system: &mut FontSystem, metrics: Metrics) -> CellSize {
    let mut buf = Buffer::new_empty(metrics);
    let attrs = Attrs::new().family(Family::Monospace);
    buf.set_text(font_system, "M", &attrs, Shaping::Advanced, None);

    // Advance width from the first glyph of the first layout run.
    let advance = buf
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first())
        .map_or(metrics.font_size, |g| g.w);

    CellSize { width: advance, height: metrics.line_height }
}

/// Fill the atlas texture with transparent black so it is well-defined.
fn clear_texture(queue: &Queue, texture: &wgpu::Texture, size: u32) {
    let pixel_count = (size * size) as usize;
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
