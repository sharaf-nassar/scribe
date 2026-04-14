use std::fmt;
use std::io;
use std::path::Path;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use scribe_common::screen::{ScreenCell, ScreenColor, ScreenSnapshot};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CELL_WIDTH: u32 = 10;
const CELL_HEIGHT: u32 = 20;
const FONT_SIZE_PX: i32 = 14;
const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT_FACTOR: f32 = 1.2;
/// Bytes per RGBA pixel.
const BPP: usize = 4;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during PNG rendering.
#[derive(Debug)]
pub enum RenderError {
    /// An I/O error (file creation, buffered writes, etc.).
    Io(io::Error),
    /// The PNG encoder failed.
    PngEncode(png::EncodingError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "render I/O error: {err}"),
            Self::PngEncode(err) => write!(f, "PNG encoding error: {err}"),
        }
    }
}

impl std::error::Error for RenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::PngEncode(err) => Some(err),
        }
    }
}

impl From<io::Error> for RenderError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<png::EncodingError> for RenderError {
    fn from(err: png::EncodingError) -> Self {
        Self::PngEncode(err)
    }
}

// ---------------------------------------------------------------------------
// ANSI 16-colour palette (sRGB u8 values)
// ---------------------------------------------------------------------------

/// Standard 16-colour ANSI palette as `[r, g, b, a]`.
static ANSI_16: [[u8; 4]; 16] = [
    [0x00, 0x00, 0x00, 0xFF], // 0  black
    [0xAA, 0x00, 0x00, 0xFF], // 1  red
    [0x00, 0xAA, 0x00, 0xFF], // 2  green
    [0xAA, 0x55, 0x00, 0xFF], // 3  yellow
    [0x00, 0x00, 0xAA, 0xFF], // 4  blue
    [0xAA, 0x00, 0xAA, 0xFF], // 5  magenta
    [0x00, 0xAA, 0xAA, 0xFF], // 6  cyan
    [0xAA, 0xAA, 0xAA, 0xFF], // 7  white
    [0x55, 0x55, 0x55, 0xFF], // 8  bright black
    [0xFF, 0x55, 0x55, 0xFF], // 9  bright red
    [0x55, 0xFF, 0x55, 0xFF], // 10 bright green
    [0xFF, 0xFF, 0x55, 0xFF], // 11 bright yellow
    [0x55, 0x55, 0xFF, 0xFF], // 12 bright blue
    [0xFF, 0x55, 0xFF, 0xFF], // 13 bright magenta
    [0x55, 0xFF, 0xFF, 0xFF], // 14 bright cyan
    [0xFF, 0xFF, 0xFF, 0xFF], // 15 bright white
];

// ---------------------------------------------------------------------------
// xterm 256-colour palette
// ---------------------------------------------------------------------------

/// Component intensities for the 6x6x6 colour cube (indices 16-231).
const CUBE_INTENSITIES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Build the full 256-colour xterm palette.
fn build_xterm_256() -> [[u8; 4]; 256] {
    let mut table = [[0u8; 4]; 256];

    // 0-15: copy from ANSI_16
    for (i, color) in ANSI_16.iter().enumerate() {
        if let Some(slot) = table.get_mut(i) {
            *slot = *color;
        }
    }

    // 16-231: 6x6x6 colour cube
    fill_cube(&mut table);

    // 232-255: greyscale ramp
    fill_greyscale(&mut table);

    table
}

/// Build one colour-cube entry from cube coordinates.
fn cube_entry(ri: usize, gi: usize, bi: usize) -> [u8; 4] {
    let rv = CUBE_INTENSITIES.get(ri).copied().unwrap_or(0);
    let gv = CUBE_INTENSITIES.get(gi).copied().unwrap_or(0);
    let bv = CUBE_INTENSITIES.get(bi).copied().unwrap_or(0);
    [rv, gv, bv, 0xFF]
}

/// Populate one row of the colour cube (one r,g pair, all 6 blue values).
fn fill_cube_row(table: &mut [[u8; 4]; 256], idx: &mut usize, ri: usize, gi: usize) {
    for bi in 0_usize..6 {
        if let Some(slot) = table.get_mut(*idx) {
            *slot = cube_entry(ri, gi, bi);
        }
        *idx += 1;
    }
}

/// Populate the 6x6x6 colour-cube region (entries 16-231).
fn fill_cube(table: &mut [[u8; 4]; 256]) {
    let mut idx: usize = 16;
    for ri in 0_usize..6 {
        for gi in 0_usize..6 {
            fill_cube_row(table, &mut idx, ri, gi);
        }
    }
}

/// Populate the greyscale ramp (entries 232-255).
fn fill_greyscale(table: &mut [[u8; 4]; 256]) {
    for i in 0_usize..24 {
        let step = u8::try_from(i).unwrap_or(u8::MAX);
        let val = 8_u8.saturating_add(step.saturating_mul(10));
        if let Some(slot) = table.get_mut(232 + i) {
            *slot = [val, val, val, 0xFF];
        }
    }
}

// ---------------------------------------------------------------------------
// Colour resolution
// ---------------------------------------------------------------------------

/// Fallback colour: opaque white.
const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// Resolve a `ScreenColor` to an RGBA `[u8; 4]`.
fn resolve_color(color: ScreenColor, palette: &[[u8; 4]; 256]) -> [u8; 4] {
    match color {
        ScreenColor::Named(idx) => ANSI_16.get(usize::from(idx)).copied().unwrap_or(WHITE),
        ScreenColor::Indexed(idx) => palette.get(usize::from(idx)).copied().unwrap_or(WHITE),
        ScreenColor::Rgb { r, g, b } => [r, g, b, 0xFF],
    }
}

/// Determine foreground and background colours for a cell, handling inverse
/// and dim flags.
fn resolve_cell_colors(cell: &ScreenCell, palette: &[[u8; 4]; 256]) -> ([u8; 4], [u8; 4]) {
    let mut fg = resolve_color(cell.fg, palette);
    let mut bg = resolve_color(cell.bg, palette);

    if cell.flags.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }

    if cell.flags.dim() {
        fg = dim_color(fg);
    }

    (fg, bg)
}

/// Halve the RGB brightness of a colour (simple dim approximation).
fn dim_color(rgba: [u8; 4]) -> [u8; 4] {
    let red = rgba.first().copied().unwrap_or(0) / 2;
    let green = rgba.get(1).copied().unwrap_or(0) / 2;
    let blue = rgba.get(2).copied().unwrap_or(0) / 2;
    let alpha = rgba.get(3).copied().unwrap_or(0xFF);
    [red, green, blue, alpha]
}

// ---------------------------------------------------------------------------
// Pixel buffer helpers
// ---------------------------------------------------------------------------

/// Parameters for `fill_rect` (keeps param count under the threshold).
struct FillParams {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: [u8; 4],
}

/// Fill a rectangle in the pixel buffer.
fn fill_rect(pixels: &mut [u8], img_width: u32, params: &FillParams) {
    for row in 0..params.h {
        let py = params.y + row;
        for col in 0..params.w {
            let px = params.x + col;
            set_pixel(pixels, img_width, px, py, params.color);
        }
    }
}

/// Write a single RGBA pixel at (x, y), bounds-checked.
fn set_pixel(pixels: &mut [u8], img_width: u32, px: u32, py: u32, color: [u8; 4]) {
    let offset = ((py * img_width + px) as usize) * BPP;
    if let Some(dest) = pixels.get_mut(offset..offset + BPP) {
        dest.copy_from_slice(&color);
    }
}

// ---------------------------------------------------------------------------
// Glyph rendering via cosmic-text
// ---------------------------------------------------------------------------

/// Parameters for glyph rendering (keeps param count under the threshold).
struct GlyphParams {
    ch: char,
    fg: [u8; 4],
    px: u32,
    py: u32,
    bold: bool,
    italic: bool,
}

/// Rendering context holding font system and swash cache (avoids passing
/// both as separate parameters everywhere).
struct RenderCtx {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

/// Shape a single character and return its `CacheKey`.
fn shape_cache_key(ctx: &mut RenderCtx, params: &GlyphParams) -> Option<cosmic_text::CacheKey> {
    let metrics = Metrics::new(FONT_SIZE, FONT_SIZE * LINE_HEIGHT_FACTOR);
    let mut buf = Buffer::new_empty(metrics);
    let attrs = build_attrs(params);
    let mut char_buf = [0u8; 4];
    let text = params.ch.encode_utf8(&mut char_buf);
    buf.set_text(&mut ctx.font_system, text, &attrs, Shaping::Advanced, None);

    buf.layout_runs()
        .next()
        .and_then(|run| run.glyphs.first())
        .map(|glyph| glyph.physical((0.0, 0.0), 1.0).cache_key)
}

/// Build cosmic-text `Attrs` for the given style flags.
fn build_attrs(params: &GlyphParams) -> Attrs<'static> {
    use cosmic_text::{Style, Weight};
    Attrs::new()
        .family(Family::Monospace)
        .weight(if params.bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if params.italic { Style::Italic } else { Style::Normal })
}

/// Rasterise a glyph and blit it into the pixel buffer.
fn render_glyph(ctx: &mut RenderCtx, pixels: &mut [u8], img_width: u32, params: &GlyphParams) {
    let Some(cache_key) = shape_cache_key(ctx, params) else {
        return;
    };

    let image_parts =
        ctx.swash_cache.get_image(&mut ctx.font_system, cache_key).as_ref().map(|img| {
            (
                img.placement.width,
                img.placement.height,
                img.placement.left,
                img.placement.top,
                img.content,
                img.data.clone(),
            )
        });

    let Some((gw, gh, left, top, content, data)) = image_parts else {
        return;
    };

    if gw == 0 || gh == 0 {
        return;
    }

    let Some(glyph_rgba) = content_to_rgba(content, data) else {
        return;
    };

    let blit = BlitParams { gw, gh, left, top };
    blit_glyph_pixels(pixels, img_width, params, &glyph_rgba, &blit);
}

/// Convert swash image content to a flat RGBA byte vector.
fn content_to_rgba(content: SwashContent, data: Vec<u8>) -> Option<Vec<u8>> {
    match content {
        SwashContent::Mask => {
            let mut out = Vec::with_capacity(data.len() * BPP);
            for alpha in &data {
                out.extend_from_slice(&[0xFF, 0xFF, 0xFF, *alpha]);
            }
            Some(out)
        }
        SwashContent::Color => Some(data),
        SwashContent::SubpixelMask => None,
    }
}

/// Glyph source geometry for blitting.
struct BlitParams {
    gw: u32,
    gh: u32,
    left: i32,
    top: i32,
}

/// Blit rasterised glyph pixels into the output buffer.
fn blit_glyph_pixels(
    pixels: &mut [u8],
    img_width: u32,
    params: &GlyphParams,
    glyph_rgba: &[u8],
    blit: &BlitParams,
) {
    let dest_x = params.px + u32::try_from(blit.left.max(0)).unwrap_or(u32::MAX);
    let baseline_offset = FONT_SIZE_PX.saturating_sub(blit.top).max(0);
    let dest_y = params.py + u32::try_from(baseline_offset).unwrap_or(u32::MAX);

    for gy in 0..blit.gh {
        let cy = dest_y + gy;
        for gx in 0..blit.gw {
            let cx = dest_x + gx;
            let si = (gy * blit.gw + gx) as usize * BPP;
            if let Some(src_px) = glyph_rgba.get(si..si + BPP) {
                let args = BlendArgs { img_width, px: cx, py: cy, fg: params.fg, src: src_px };
                blend_pixel(pixels, &args);
            }
        }
    }
}

/// Arguments for alpha-blending a single pixel.
struct BlendArgs<'a> {
    img_width: u32,
    px: u32,
    py: u32,
    fg: [u8; 4],
    src: &'a [u8],
}

/// Alpha-blend a glyph source pixel onto the destination, applying the
/// foreground colour tint.
fn blend_pixel(pixels: &mut [u8], args: &BlendArgs<'_>) {
    let alpha = args.src.get(3).copied().unwrap_or(0);
    if alpha == 0 {
        return;
    }
    let sr = args.fg.first().copied().unwrap_or(0xFF);
    let sg = args.fg.get(1).copied().unwrap_or(0xFF);
    let sb = args.fg.get(2).copied().unwrap_or(0xFF);

    let offset = ((args.py * args.img_width + args.px) as usize) * BPP;
    let Some(dest) = pixels.get_mut(offset..offset + BPP) else {
        return;
    };

    let dr = dest.first().copied().unwrap_or(0);
    let dg = dest.get(1).copied().unwrap_or(0);
    let db = dest.get(2).copied().unwrap_or(0);

    let al = u16::from(alpha);
    let inv_al = 255 - al;
    let mix = |s: u8, d: u8| -> u8 {
        let blended = (u16::from(s) * al + u16::from(d) * inv_al) / 255;
        u8::try_from(blended).unwrap_or(u8::MAX)
    };

    dest.copy_from_slice(&[mix(sr, dr), mix(sg, dg), mix(sb, db), 0xFF]);
}

// ---------------------------------------------------------------------------
// Cursor drawing
// ---------------------------------------------------------------------------

/// Position and colour for cursor drawing.
struct CursorPos {
    cx: u32,
    cy: u32,
    color: [u8; 4],
}

/// Draw a cursor indicator at the cursor position.
fn draw_cursor(pixels: &mut [u8], img_width: u32, snapshot: &ScreenSnapshot) {
    let pos = CursorPos {
        cx: u32::from(snapshot.cursor_col) * CELL_WIDTH,
        cy: u32::from(snapshot.cursor_row) * CELL_HEIGHT,
        color: [0xCC, 0xCC, 0xCC, 0xFF],
    };

    match snapshot.cursor_style {
        scribe_common::screen::CursorStyle::Block => {
            draw_cursor_block(pixels, img_width, &pos);
        }
        scribe_common::screen::CursorStyle::Beam => {
            let params =
                FillParams { x: pos.cx, y: pos.cy, w: 2, h: CELL_HEIGHT, color: pos.color };
            fill_rect(pixels, img_width, &params);
        }
        scribe_common::screen::CursorStyle::Underline => {
            draw_cursor_underline(pixels, img_width, &pos);
        }
        scribe_common::screen::CursorStyle::HollowBlock => {
            draw_cursor_hollow(pixels, img_width, &pos);
        }
    }
}

/// Draw a cursor underline (2px high bar at the bottom of the cell).
fn draw_cursor_underline(pixels: &mut [u8], img_width: u32, pos: &CursorPos) {
    let params = FillParams {
        x: pos.cx,
        y: pos.cy + CELL_HEIGHT.saturating_sub(2),
        w: CELL_WIDTH,
        h: 2,
        color: pos.color,
    };
    fill_rect(pixels, img_width, &params);
}

/// Draw a solid block cursor (invert existing pixels for contrast).
fn draw_cursor_block(pixels: &mut [u8], img_width: u32, pos: &CursorPos) {
    for row in 0..CELL_HEIGHT {
        for col in 0..CELL_WIDTH {
            invert_or_set(pixels, img_width, pos.cx + col, pos.cy + row, pos.color);
        }
    }
}

/// Draw a hollow block cursor (outline only).
fn draw_cursor_hollow(pixels: &mut [u8], img_width: u32, pos: &CursorPos) {
    for col in 0..CELL_WIDTH {
        set_pixel(pixels, img_width, pos.cx + col, pos.cy, pos.color);
        set_pixel(
            pixels,
            img_width,
            pos.cx + col,
            pos.cy + CELL_HEIGHT.saturating_sub(1),
            pos.color,
        );
    }
    for row in 1..CELL_HEIGHT.saturating_sub(1) {
        set_pixel(pixels, img_width, pos.cx, pos.cy + row, pos.color);
        set_pixel(
            pixels,
            img_width,
            pos.cx + CELL_WIDTH.saturating_sub(1),
            pos.cy + row,
            pos.color,
        );
    }
}

/// Invert existing pixel or set to `color` if destination is near-black.
fn invert_or_set(pixels: &mut [u8], img_width: u32, px: u32, py: u32, color: [u8; 4]) {
    let offset = ((py * img_width + px) as usize) * BPP;
    let Some(dest) = pixels.get_mut(offset..offset + BPP) else {
        return;
    };
    let red = dest.first().copied().unwrap_or(0);
    let green = dest.get(1).copied().unwrap_or(0);
    let blue = dest.get(2).copied().unwrap_or(0);

    if red < 10 && green < 10 && blue < 10 {
        dest.copy_from_slice(&color);
    } else {
        dest.copy_from_slice(&[255 - red, 255 - green, 255 - blue, 0xFF]);
    }
}

// ---------------------------------------------------------------------------
// Cell rendering
// ---------------------------------------------------------------------------

/// State needed to render a single cell.
struct CellRenderArgs<'a> {
    pixels: &'a mut [u8],
    img_width: u32,
    ctx: &'a mut RenderCtx,
    palette: &'a [[u8; 4]; 256],
}

/// Render all cells from the snapshot into the pixel buffer.
fn render_cells(
    snapshot: &ScreenSnapshot,
    pixels: &mut [u8],
    img_width: u32,
    ctx: &mut RenderCtx,
    palette: &[[u8; 4]; 256],
) {
    let cols = usize::from(snapshot.cols);
    let mut args = CellRenderArgs { pixels, img_width, ctx, palette };
    for (idx, cell) in snapshot.cells.iter().enumerate() {
        render_single_cell(&mut args, cell, idx, cols);
    }
}

/// Render one cell: fill background and draw glyph if needed.
fn render_single_cell(args: &mut CellRenderArgs<'_>, cell: &ScreenCell, idx: usize, cols: usize) {
    if cols == 0 {
        return;
    }
    let col = u32::try_from(idx % cols).unwrap_or(u32::MAX);
    let row = u32::try_from(idx / cols).unwrap_or(u32::MAX);

    let (fg, bg) = resolve_cell_colors(cell, args.palette);
    let fill = FillParams {
        x: col * CELL_WIDTH,
        y: row * CELL_HEIGHT,
        w: CELL_WIDTH,
        h: CELL_HEIGHT,
        color: bg,
    };
    fill_rect(args.pixels, args.img_width, &fill);

    if cell.c != ' ' && !cell.flags.hidden() {
        let glyph = GlyphParams {
            ch: cell.c,
            fg,
            px: col * CELL_WIDTH,
            py: row * CELL_HEIGHT,
            bold: cell.flags.bold(),
            italic: cell.flags.italic(),
        };
        render_glyph(args.ctx, args.pixels, args.img_width, &glyph);
    }
}

// ---------------------------------------------------------------------------
// PNG writing
// ---------------------------------------------------------------------------

/// Encode an RGBA pixel buffer as a PNG file.
fn write_png(path: &Path, pixels: &[u8], width: u32, height: u32) -> Result<(), RenderError> {
    let file = std::fs::File::create(path)?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;
    png_writer.write_image_data(pixels)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Render a `ScreenSnapshot` to a PNG file at `path`.
///
/// Uses `cosmic-text` for glyph rasterisation and the `png` crate for
/// encoding. No GPU required.
pub fn render_to_png(snapshot: &ScreenSnapshot, path: &Path) -> Result<(), RenderError> {
    let width = u32::from(snapshot.cols) * CELL_WIDTH;
    let height = u32::from(snapshot.rows) * CELL_HEIGHT;
    let mut pixels = vec![0u8; (width * height) as usize * BPP];

    let mut ctx = RenderCtx { font_system: FontSystem::new(), swash_cache: SwashCache::new() };

    let palette = build_xterm_256();
    render_cells(snapshot, &mut pixels, width, &mut ctx, &palette);

    if snapshot.cursor_visible {
        draw_cursor(&mut pixels, width, snapshot);
    }

    write_png(path, &pixels, width, height)
}
