//! Procedural renderer for Unicode box-drawing characters (U+2500–U+257F)
//! and block elements (U+2580–U+259F).
//!
//! Terminal fonts often leave sub-pixel gaps between adjacent box-drawing
//! glyphs because font bearings do not extend to cell edges. This module
//! draws these characters as pixel-perfect geometric shapes that fill the
//! cell edge-to-edge, guaranteeing seamless tiling.
//!
//! Each function produces an RGBA canvas of exactly `cell_w × cell_h` pixels
//! with white foreground (alpha encodes shape) — the GPU fragment shader
//! applies the actual fg/bg colours via `mix(bg, fg, alpha)`.

/// Returns `true` if `c` should be rendered procedurally instead of from
/// the font.
pub const fn is_box_drawing(c: char) -> bool {
    matches!(c,
        '\u{2500}'..='\u{257F}' | // Box Drawing
        '\u{2580}'..='\u{259F}'   // Block Elements
    )
}

/// Render a box-drawing or block-element character into an RGBA canvas.
///
/// Returns `Some((width, height, rgba))` or `None` if the character is not
/// handled (caller should fall back to font rasterisation).
pub fn render(c: char, cell_w: u32, cell_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    if cell_w == 0 || cell_h == 0 {
        return None;
    }

    let mut canvas = vec![0u8; (cell_w * cell_h * 4) as usize];

    match c {
        '\u{2580}'..='\u{259F}' => render_block(c, cell_w, cell_h, &mut canvas)?,
        '\u{2500}'..='\u{257F}' => render_box(c, cell_w, cell_h, &mut canvas)?,
        _ => return None,
    }

    Some((cell_w, cell_h, canvas))
}

// ─── Block Elements ──────────────────────────────────────────────────

/// Render a block element character.
fn render_block(c: char, cw: u32, ch: u32, canvas: &mut [u8]) -> Option<()> {
    match c {
        // ▀ Upper half block
        '\u{2580}' => fill_rect(canvas, cw, [0, 0, cw, ch / 2]),
        // ▁–▇ Lower 1/8 through 7/8
        '\u{2581}' => fill_rect(canvas, cw, [0, ch - ch / 8, cw, ch / 8]),
        '\u{2582}' => fill_rect(canvas, cw, [0, ch - ch / 4, cw, ch / 4]),
        '\u{2583}' => fill_rect(canvas, cw, [0, ch - (3 * ch / 8), cw, 3 * ch / 8]),
        '\u{2584}' => fill_rect(canvas, cw, [0, ch / 2, cw, ch - ch / 2]),
        '\u{2585}' => fill_rect(canvas, cw, [0, ch - (5 * ch / 8), cw, 5 * ch / 8]),
        '\u{2586}' => fill_rect(canvas, cw, [0, ch - (3 * ch / 4), cw, 3 * ch / 4]),
        '\u{2587}' => fill_rect(canvas, cw, [0, ch - (7 * ch / 8), cw, 7 * ch / 8]),
        // █ Full block
        '\u{2588}' => fill_rect(canvas, cw, [0, 0, cw, ch]),
        // ▉–▏ Left 7/8 through 1/8
        '\u{2589}' => fill_rect(canvas, cw, [0, 0, 7 * cw / 8, ch]),
        '\u{258A}' => fill_rect(canvas, cw, [0, 0, 3 * cw / 4, ch]),
        '\u{258B}' => fill_rect(canvas, cw, [0, 0, 5 * cw / 8, ch]),
        '\u{258C}' => fill_rect(canvas, cw, [0, 0, cw / 2, ch]),
        '\u{258D}' => fill_rect(canvas, cw, [0, 0, 3 * cw / 8, ch]),
        '\u{258E}' => fill_rect(canvas, cw, [0, 0, cw / 4, ch]),
        '\u{258F}' => fill_rect(canvas, cw, [0, 0, cw / 8, ch]),
        // ▐ Right half block
        '\u{2590}' => fill_rect(canvas, cw, [cw / 2, 0, cw - cw / 2, ch]),
        // ░ Light shade (25%)
        '\u{2591}' => shade(canvas, cw, ch, 64),
        // ▒ Medium shade (50%)
        '\u{2592}' => shade(canvas, cw, ch, 128),
        // ▓ Dark shade (75%)
        '\u{2593}' => shade(canvas, cw, ch, 192),
        // ▔ Upper 1/8 block
        '\u{2594}' => fill_rect(canvas, cw, [0, 0, cw, ch / 8]),
        // ▕ Right 1/8 block
        '\u{2595}' => fill_rect(canvas, cw, [cw - cw / 8, 0, cw / 8, ch]),
        // ▖ Quadrant lower left
        '\u{2596}' => fill_rect(canvas, cw, [0, ch / 2, cw / 2, ch - ch / 2]),
        // ▗ Quadrant lower right
        '\u{2597}' => fill_rect(canvas, cw, [cw / 2, ch / 2, cw - cw / 2, ch - ch / 2]),
        // ▘ Quadrant upper left
        '\u{2598}' => fill_rect(canvas, cw, [0, 0, cw / 2, ch / 2]),
        // ▙ Quadrant upper left + lower left + lower right
        '\u{2599}' => {
            fill_rect(canvas, cw, [0, 0, cw / 2, ch / 2]);
            fill_rect(canvas, cw, [0, ch / 2, cw, ch - ch / 2]);
        }
        // ▚ Quadrant upper left + lower right
        '\u{259A}' => {
            fill_rect(canvas, cw, [0, 0, cw / 2, ch / 2]);
            fill_rect(canvas, cw, [cw / 2, ch / 2, cw - cw / 2, ch - ch / 2]);
        }
        // ▛ Quadrant upper left + upper right + lower left
        '\u{259B}' => {
            fill_rect(canvas, cw, [0, 0, cw, ch / 2]);
            fill_rect(canvas, cw, [0, ch / 2, cw / 2, ch - ch / 2]);
        }
        // ▜ Quadrant upper left + upper right + lower right
        '\u{259C}' => {
            fill_rect(canvas, cw, [0, 0, cw, ch / 2]);
            fill_rect(canvas, cw, [cw / 2, ch / 2, cw - cw / 2, ch - ch / 2]);
        }
        // ▝ Quadrant upper right
        '\u{259D}' => fill_rect(canvas, cw, [cw / 2, 0, cw - cw / 2, ch / 2]),
        // ▞ Quadrant upper right + lower left
        '\u{259E}' => {
            fill_rect(canvas, cw, [cw / 2, 0, cw - cw / 2, ch / 2]);
            fill_rect(canvas, cw, [0, ch / 2, cw / 2, ch - ch / 2]);
        }
        // ▟ Quadrant upper right + lower left + lower right
        '\u{259F}' => {
            fill_rect(canvas, cw, [cw / 2, 0, cw - cw / 2, ch / 2]);
            fill_rect(canvas, cw, [0, ch / 2, cw, ch - ch / 2]);
        }
        _ => return None,
    }

    Some(())
}

// ─── Box Drawing ─────────────────────────────────────────────────────

/// Segment directions for box-drawing line composition.
#[derive(Clone, Copy)]
struct Segments {
    up: LineWeight,
    down: LineWeight,
    left: LineWeight,
    right: LineWeight,
}

/// Weight of a box-drawing line segment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineWeight {
    None,
    Light,
    Heavy,
    Double,
    /// Light dashed (renders same as Light for the segment itself).
    LightDash,
    /// Heavy dashed.
    HeavyDash,
}

impl LineWeight {
    const fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Pre-computed line geometry dimensions shared by all four segments.
struct LineMetrics {
    stride: u32,
    thin: u32,
    thick: u32,
    dbl_gap: u32,
    dbl_stroke: u32,
}

/// Decode a box-drawing codepoint into its four directional segments.
///
/// Returns `None` for characters that are not simple line compositions
/// (e.g. diagonal lines return `None`; rounded corners map to light).
#[allow(
    clippy::too_many_lines,
    reason = "exhaustive match over 128 box-drawing codepoints is inherently long"
)]
fn decode_segments(c: char) -> Option<Segments> {
    use LineWeight::{Double, Heavy, HeavyDash, Light, LightDash, None as N};

    let seg = match c {
        // ─ ━ │ ┃  Solid horizontal/vertical
        '\u{2500}' => Segments { left: Light, right: Light, up: N, down: N },
        '\u{2501}' => Segments { left: Heavy, right: Heavy, up: N, down: N },
        '\u{2502}' => Segments { left: N, right: N, up: Light, down: Light },
        '\u{2503}' => Segments { left: N, right: N, up: Heavy, down: Heavy },
        // Dashed horizontal (triple, quadruple, double-dash — all map to same rendering)
        '\u{2504}' | '\u{2508}' | '\u{254C}' => {
            Segments { left: LightDash, right: LightDash, up: N, down: N }
        }
        '\u{2505}' | '\u{2509}' | '\u{254D}' => {
            Segments { left: HeavyDash, right: HeavyDash, up: N, down: N }
        }
        // Dashed vertical
        '\u{2506}' | '\u{250A}' | '\u{254E}' => {
            Segments { left: N, right: N, up: LightDash, down: LightDash }
        }
        '\u{2507}' | '\u{250B}' | '\u{254F}' => {
            Segments { left: N, right: N, up: HeavyDash, down: HeavyDash }
        }
        // ┌ Down-and-right corners (╭ rounded maps to light)
        '\u{250C}' | '\u{256D}' => Segments { left: N, right: Light, up: N, down: Light },
        '\u{250D}' => Segments { left: N, right: Heavy, up: N, down: Light },
        '\u{250E}' => Segments { left: N, right: Light, up: N, down: Heavy },
        '\u{250F}' => Segments { left: N, right: Heavy, up: N, down: Heavy },
        // ┐ Down-and-left corners (╮ rounded maps to light)
        '\u{2510}' | '\u{256E}' => Segments { left: Light, right: N, up: N, down: Light },
        '\u{2511}' => Segments { left: Heavy, right: N, up: N, down: Light },
        '\u{2512}' => Segments { left: Light, right: N, up: N, down: Heavy },
        '\u{2513}' => Segments { left: Heavy, right: N, up: N, down: Heavy },
        // └ Up-and-right corners (╰ rounded maps to light)
        '\u{2514}' | '\u{2570}' => Segments { left: N, right: Light, up: Light, down: N },
        '\u{2515}' => Segments { left: N, right: Heavy, up: Light, down: N },
        '\u{2516}' => Segments { left: N, right: Light, up: Heavy, down: N },
        '\u{2517}' => Segments { left: N, right: Heavy, up: Heavy, down: N },
        // ┘ Up-and-left corners (╯ rounded maps to light)
        '\u{2518}' | '\u{256F}' => Segments { left: Light, right: N, up: Light, down: N },
        '\u{2519}' => Segments { left: Heavy, right: N, up: Light, down: N },
        '\u{251A}' => Segments { left: Light, right: N, up: Heavy, down: N },
        '\u{251B}' => Segments { left: Heavy, right: N, up: Heavy, down: N },
        // ├ Vertical-and-right (T-pieces)
        '\u{251C}' => Segments { left: N, right: Light, up: Light, down: Light },
        '\u{251D}' => Segments { left: N, right: Heavy, up: Light, down: Light },
        '\u{251E}' => Segments { left: N, right: Light, up: Heavy, down: Light },
        '\u{251F}' => Segments { left: N, right: Light, up: Light, down: Heavy },
        '\u{2520}' => Segments { left: N, right: Light, up: Heavy, down: Heavy },
        '\u{2521}' => Segments { left: N, right: Heavy, up: Heavy, down: Light },
        '\u{2522}' => Segments { left: N, right: Heavy, up: Light, down: Heavy },
        '\u{2523}' => Segments { left: N, right: Heavy, up: Heavy, down: Heavy },
        // ┤ Vertical-and-left (T-pieces)
        '\u{2524}' => Segments { left: Light, right: N, up: Light, down: Light },
        '\u{2525}' => Segments { left: Heavy, right: N, up: Light, down: Light },
        '\u{2526}' => Segments { left: Light, right: N, up: Heavy, down: Light },
        '\u{2527}' => Segments { left: Light, right: N, up: Light, down: Heavy },
        '\u{2528}' => Segments { left: Light, right: N, up: Heavy, down: Heavy },
        '\u{2529}' => Segments { left: Heavy, right: N, up: Heavy, down: Light },
        '\u{252A}' => Segments { left: Heavy, right: N, up: Light, down: Heavy },
        '\u{252B}' => Segments { left: Heavy, right: N, up: Heavy, down: Heavy },
        // ┬ Down-and-horizontal (T-pieces)
        '\u{252C}' => Segments { left: Light, right: Light, up: N, down: Light },
        '\u{252D}' => Segments { left: Heavy, right: Light, up: N, down: Light },
        '\u{252E}' => Segments { left: Light, right: Heavy, up: N, down: Light },
        '\u{252F}' => Segments { left: Heavy, right: Heavy, up: N, down: Light },
        '\u{2530}' => Segments { left: Light, right: Light, up: N, down: Heavy },
        '\u{2531}' => Segments { left: Heavy, right: Light, up: N, down: Heavy },
        '\u{2532}' => Segments { left: Light, right: Heavy, up: N, down: Heavy },
        '\u{2533}' => Segments { left: Heavy, right: Heavy, up: N, down: Heavy },
        // ┴ Up-and-horizontal (T-pieces)
        '\u{2534}' => Segments { left: Light, right: Light, up: Light, down: N },
        '\u{2535}' => Segments { left: Heavy, right: Light, up: Light, down: N },
        '\u{2536}' => Segments { left: Light, right: Heavy, up: Light, down: N },
        '\u{2537}' => Segments { left: Heavy, right: Heavy, up: Light, down: N },
        '\u{2538}' => Segments { left: Light, right: Light, up: Heavy, down: N },
        '\u{2539}' => Segments { left: Heavy, right: Light, up: Heavy, down: N },
        '\u{253A}' => Segments { left: Light, right: Heavy, up: Heavy, down: N },
        '\u{253B}' => Segments { left: Heavy, right: Heavy, up: Heavy, down: N },
        // ┼ Cross pieces
        '\u{253C}' => Segments { left: Light, right: Light, up: Light, down: Light },
        '\u{253D}' => Segments { left: Heavy, right: Light, up: Light, down: Light },
        '\u{253E}' => Segments { left: Light, right: Heavy, up: Light, down: Light },
        '\u{253F}' => Segments { left: Heavy, right: Heavy, up: Light, down: Light },
        '\u{2540}' => Segments { left: Light, right: Light, up: Heavy, down: Light },
        '\u{2541}' => Segments { left: Light, right: Light, up: Light, down: Heavy },
        '\u{2542}' => Segments { left: Light, right: Light, up: Heavy, down: Heavy },
        '\u{2543}' => Segments { left: Heavy, right: Light, up: Heavy, down: Light },
        '\u{2544}' => Segments { left: Light, right: Heavy, up: Heavy, down: Light },
        '\u{2545}' => Segments { left: Heavy, right: Light, up: Light, down: Heavy },
        '\u{2546}' => Segments { left: Light, right: Heavy, up: Light, down: Heavy },
        '\u{2547}' => Segments { left: Heavy, right: Heavy, up: Heavy, down: Light },
        '\u{2548}' => Segments { left: Heavy, right: Heavy, up: Light, down: Heavy },
        '\u{2549}' => Segments { left: Heavy, right: Light, up: Heavy, down: Heavy },
        '\u{254A}' => Segments { left: Light, right: Heavy, up: Heavy, down: Heavy },
        '\u{254B}' => Segments { left: Heavy, right: Heavy, up: Heavy, down: Heavy },
        // ═ ║ Double lines
        '\u{2550}' => Segments { left: Double, right: Double, up: N, down: N },
        '\u{2551}' => Segments { left: N, right: N, up: Double, down: Double },
        // ╒╓╔ Double corners (down-right)
        '\u{2552}' => Segments { left: N, right: Double, up: N, down: Light },
        '\u{2553}' => Segments { left: N, right: Light, up: N, down: Double },
        '\u{2554}' => Segments { left: N, right: Double, up: N, down: Double },
        // ╕╖╗ Double corners (down-left)
        '\u{2555}' => Segments { left: Double, right: N, up: N, down: Light },
        '\u{2556}' => Segments { left: Light, right: N, up: N, down: Double },
        '\u{2557}' => Segments { left: Double, right: N, up: N, down: Double },
        // ╘╙╚ Double corners (up-right)
        '\u{2558}' => Segments { left: N, right: Double, up: Light, down: N },
        '\u{2559}' => Segments { left: N, right: Light, up: Double, down: N },
        '\u{255A}' => Segments { left: N, right: Double, up: Double, down: N },
        // ╛╜╝ Double corners (up-left)
        '\u{255B}' => Segments { left: Double, right: N, up: Light, down: N },
        '\u{255C}' => Segments { left: Light, right: N, up: Double, down: N },
        '\u{255D}' => Segments { left: Double, right: N, up: Double, down: N },
        // ╞╟╠ Double T-pieces (vertical-and-right)
        '\u{255E}' => Segments { left: N, right: Double, up: Light, down: Light },
        '\u{255F}' => Segments { left: N, right: Light, up: Double, down: Double },
        '\u{2560}' => Segments { left: N, right: Double, up: Double, down: Double },
        // ╡╢╣ Double T-pieces (vertical-and-left)
        '\u{2561}' => Segments { left: Double, right: N, up: Light, down: Light },
        '\u{2562}' => Segments { left: Light, right: N, up: Double, down: Double },
        '\u{2563}' => Segments { left: Double, right: N, up: Double, down: Double },
        // ╤╥╦ Double T-pieces (down-and-horizontal)
        '\u{2564}' => Segments { left: Double, right: Double, up: N, down: Light },
        '\u{2565}' => Segments { left: Light, right: Light, up: N, down: Double },
        '\u{2566}' => Segments { left: Double, right: Double, up: N, down: Double },
        // ╧╨╩ Double T-pieces (up-and-horizontal)
        '\u{2567}' => Segments { left: Double, right: Double, up: Light, down: N },
        '\u{2568}' => Segments { left: Light, right: Light, up: Double, down: N },
        '\u{2569}' => Segments { left: Double, right: Double, up: Double, down: N },
        // ╪╫╬ Double cross pieces
        '\u{256A}' => Segments { left: Double, right: Double, up: Light, down: Light },
        '\u{256B}' => Segments { left: Light, right: Light, up: Double, down: Double },
        '\u{256C}' => Segments { left: Double, right: Double, up: Double, down: Double },
        // ╴╵╶╷ Light half-lines
        '\u{2574}' => Segments { left: Light, right: N, up: N, down: N },
        '\u{2575}' => Segments { left: N, right: N, up: Light, down: N },
        '\u{2576}' => Segments { left: N, right: Light, up: N, down: N },
        '\u{2577}' => Segments { left: N, right: N, up: N, down: Light },
        // ╸╹╺╻ Heavy half-lines
        '\u{2578}' => Segments { left: Heavy, right: N, up: N, down: N },
        '\u{2579}' => Segments { left: N, right: N, up: Heavy, down: N },
        '\u{257A}' => Segments { left: N, right: Heavy, up: N, down: N },
        '\u{257B}' => Segments { left: N, right: N, up: N, down: Heavy },
        // ╼╽╾╿ Mixed half-lines
        '\u{257C}' => Segments { left: Light, right: Heavy, up: N, down: N },
        '\u{257D}' => Segments { left: N, right: N, up: Light, down: Heavy },
        '\u{257E}' => Segments { left: Heavy, right: Light, up: N, down: N },
        '\u{257F}' => Segments { left: N, right: N, up: Heavy, down: Light },
        _ => return None,
    };

    Some(seg)
}

/// Render a box-drawing character by composing its directional segments.
fn render_box(c: char, cw: u32, ch: u32, canvas: &mut [u8]) -> Option<()> {
    let seg = decode_segments(c)?;

    let mid_x = cw / 2;
    let mid_y = ch / 2;

    let lm = LineMetrics {
        stride: cw,
        thin: (cw / 8).max(1),
        thick: (cw / 4).max(2),
        dbl_gap: (cw / 6).max(2),
        dbl_stroke: (cw / 10).max(1),
    };

    if seg.left.is_some() {
        draw_h_segment(canvas, seg.left, [0, mid_x], mid_y, &lm);
    }
    if seg.right.is_some() {
        draw_h_segment(canvas, seg.right, [mid_x, cw], mid_y, &lm);
    }
    if seg.up.is_some() {
        draw_v_segment(canvas, seg.up, [0, mid_y], mid_x, &lm);
    }
    if seg.down.is_some() {
        draw_v_segment(canvas, seg.down, [mid_y, ch], mid_x, &lm);
    }

    Some(())
}

/// Draw a horizontal segment from `span[0]` to `span[1]`, centred on `mid`.
///
/// `span` is `[x_start, x_end]`, `mid` is the vertical centre.
fn draw_h_segment(
    canvas: &mut [u8],
    weight: LineWeight,
    span: [u32; 2],
    mid: u32,
    lm: &LineMetrics,
) {
    let width = span[1] - span[0];
    match weight {
        LineWeight::None => {}
        LineWeight::Light | LineWeight::LightDash => {
            let half = lm.thin / 2;
            fill_rect(canvas, lm.stride, [span[0], mid.saturating_sub(half), width, lm.thin]);
        }
        LineWeight::Heavy | LineWeight::HeavyDash => {
            let half = lm.thick / 2;
            fill_rect(canvas, lm.stride, [span[0], mid.saturating_sub(half), width, lm.thick]);
        }
        LineWeight::Double => {
            let half_gap = lm.dbl_gap / 2;
            let y_top = mid.saturating_sub(half_gap + lm.dbl_stroke);
            let y_bot = mid + half_gap;
            fill_rect(canvas, lm.stride, [span[0], y_top, width, lm.dbl_stroke]);
            fill_rect(canvas, lm.stride, [span[0], y_bot, width, lm.dbl_stroke]);
        }
    }
}

/// Draw a vertical segment from `span[0]` to `span[1]`, centred on `mid`.
///
/// `span` is `[y_start, y_end]`, `mid` is the horizontal centre.
fn draw_v_segment(
    canvas: &mut [u8],
    weight: LineWeight,
    span: [u32; 2],
    mid: u32,
    lm: &LineMetrics,
) {
    let height = span[1] - span[0];
    match weight {
        LineWeight::None => {}
        LineWeight::Light | LineWeight::LightDash => {
            let half = lm.thin / 2;
            fill_rect(canvas, lm.stride, [mid.saturating_sub(half), span[0], lm.thin, height]);
        }
        LineWeight::Heavy | LineWeight::HeavyDash => {
            let half = lm.thick / 2;
            fill_rect(canvas, lm.stride, [mid.saturating_sub(half), span[0], lm.thick, height]);
        }
        LineWeight::Double => {
            let half_gap = lm.dbl_gap / 2;
            let x_left = mid.saturating_sub(half_gap + lm.dbl_stroke);
            let x_right = mid + half_gap;
            fill_rect(canvas, lm.stride, [x_left, span[0], lm.dbl_stroke, height]);
            fill_rect(canvas, lm.stride, [x_right, span[0], lm.dbl_stroke, height]);
        }
    }
}

// ─── Pixel helpers ───────────────────────────────────────────────────

/// Fill a rectangle `[x, y, width, height]` on an RGBA canvas with
/// opaque white (alpha = 255).
fn fill_rect(canvas: &mut [u8], stride: u32, area: [u32; 4]) {
    let [px, py, rw, rh] = area;
    for row in py..py + rh {
        for col in px..px + rw {
            if let Some(pixel) = pixel_mut(canvas, stride, col, row) {
                pixel.copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
}

/// Fill the entire canvas with a uniform alpha value (for shade characters).
fn shade(canvas: &mut [u8], cw: u32, ch: u32, alpha: u8) {
    for row in 0..ch {
        for col in 0..cw {
            if let Some(pixel) = pixel_mut(canvas, cw, col, row) {
                pixel.copy_from_slice(&[255, 255, 255, alpha]);
            }
        }
    }
}

/// Return a mutable 4-byte slice for pixel `(col, row)`, or `None` if out
/// of bounds.
fn pixel_mut(canvas: &mut [u8], stride: u32, col: u32, row: u32) -> Option<&mut [u8]> {
    let offset = ((row * stride + col) * 4) as usize;
    canvas.get_mut(offset..offset + 4)
}
