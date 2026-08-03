//! Bounded PNG-only decoder core derived from `png` 0.18.1.
//!
//! Scribe retains static PNG parsing, RFC 1950 inflate, unfiltering, Adam7,
//! and color conversion. Encoder, APNG, text, profile, and generic image paths
//! are excluded. Every untrusted work and allocation boundary uses the shared
//! caller-owned decode budget.

use std::error::Error;
use std::fmt;

use flate2::{Decompress, FlushDecompress, Status};
use scribe_image_decode::{BudgetError, DecodeBudget};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// PNG-specific hard ceilings supplied by the Kitty normalizer.
#[derive(Clone, Copy, Debug)]
pub struct PngLimits {
    pub max_width_pixels: usize,
    pub max_height_pixels: usize,
    pub max_pixels: usize,
    pub max_inflated_bytes: usize,
    pub max_rgba_bytes: usize,
}

/// Stable PNG failure details without source bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PngError {
    InvalidSignature,
    InvalidChunk,
    InvalidCrc,
    UnsupportedAnimation,
    UnsupportedColor,
    InvalidDimensions { width: usize, height: usize },
    QuotaExceeded { limit: &'static str, requested: u64, maximum: u64 },
    InflateFailed,
    InflatedLengthMismatch { expected: usize, actual: usize },
    AllocationFailed { requested_bytes: usize },
    WorkBudgetExceeded,
    DecodeDeadlineExceeded,
    DecodeCancelled,
}

impl PngError {
    pub const fn category(self) -> &'static str {
        match self {
            Self::InvalidDimensions { .. } => "invalid_dimensions",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::WorkBudgetExceeded => "work_budget_exceeded",
            Self::DecodeDeadlineExceeded => "decode_deadline_exceeded",
            Self::DecodeCancelled => "decode_cancelled",
            Self::AllocationFailed { .. }
            | Self::InflateFailed
            | Self::InflatedLengthMismatch { .. } => "decode_failed",
            Self::InvalidSignature
            | Self::InvalidChunk
            | Self::InvalidCrc
            | Self::UnsupportedAnimation
            | Self::UnsupportedColor => "malformed_payload",
        }
    }
}

impl fmt::Display for PngError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSignature => formatter.write_str("invalid PNG signature"),
            Self::InvalidChunk => formatter.write_str("invalid PNG chunk structure"),
            Self::InvalidCrc => formatter.write_str("invalid PNG chunk CRC"),
            Self::UnsupportedAnimation => formatter.write_str("APNG is unsupported"),
            Self::UnsupportedColor => formatter.write_str("unsupported PNG color encoding"),
            Self::InvalidDimensions { width, height } => {
                write!(formatter, "invalid PNG dimensions {width}x{height}")
            }
            Self::QuotaExceeded { limit, requested, maximum } => {
                write!(formatter, "PNG {limit} quota exceeded: {requested} > {maximum}")
            }
            Self::InflateFailed => formatter.write_str("PNG inflate failed"),
            Self::InflatedLengthMismatch { expected, actual } => {
                write!(formatter, "PNG inflated length mismatch: {actual} != {expected}")
            }
            Self::AllocationFailed { requested_bytes } => {
                write!(formatter, "PNG allocation failed for {requested_bytes} bytes")
            }
            Self::WorkBudgetExceeded => formatter.write_str("PNG work budget exceeded"),
            Self::DecodeDeadlineExceeded => formatter.write_str("PNG decode deadline exceeded"),
            Self::DecodeCancelled => formatter.write_str("PNG decode cancelled"),
        }
    }
}

impl Error for PngError {}

impl From<BudgetError> for PngError {
    fn from(error: BudgetError) -> Self {
        match error {
            BudgetError::InvalidLimits | BudgetError::WorkBudgetExceeded { .. } => {
                Self::WorkBudgetExceeded
            }
            BudgetError::DecodeDeadlineExceeded { .. } => Self::DecodeDeadlineExceeded,
            BudgetError::DecodeCancelled { .. } => Self::DecodeCancelled,
            BudgetError::AllocationFailed { requested_bytes } => {
                Self::AllocationFailed { requested_bytes }
            }
        }
    }
}

/// Completed static PNG normalized to canonical RGBA.
#[derive(Debug)]
pub struct DecodedPng {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub has_alpha: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ColorType {
    Grayscale,
    Rgb,
    Palette,
    GrayscaleAlpha,
    Rgba,
}

impl ColorType {
    const fn channels(self) -> usize {
        match self {
            Self::Grayscale | Self::Palette => 1,
            Self::Rgb => 3,
            Self::GrayscaleAlpha => 2,
            Self::Rgba => 4,
        }
    }
}

#[derive(Clone)]
struct Metadata {
    width: usize,
    height: usize,
    bit_depth: u8,
    color: ColorType,
    interlaced: bool,
    palette: [[u8; 3]; 256],
    palette_len: usize,
    palette_alpha: [u8; 256],
    palette_alpha_len: usize,
    transparent_gray: Option<u16>,
    transparent_rgb: Option<[u16; 3]>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            bit_depth: 0,
            color: ColorType::Grayscale,
            interlaced: false,
            palette: [[0; 3]; 256],
            palette_len: 0,
            palette_alpha: [255; 256],
            palette_alpha_len: 0,
            transparent_gray: None,
            transparent_rgb: None,
        }
    }
}

struct Parsed {
    metadata: Metadata,
    inflated_len: usize,
}

// @lat: [[terminal-images#Terminal Images#Bounded Kitty PNG Decoder]]
/// Decode one complete static PNG through bounded incremental stages.
pub fn decode_png(
    data: &[u8],
    limits: PngLimits,
    budget: &mut DecodeBudget<'_>,
) -> Result<DecodedPng, PngError> {
    let parsed = parse_png(data, limits, budget)?;
    let mut inflated = inflate_idat(data, parsed.inflated_len, budget)?;
    let rgba_len = guard_dimensions(parsed.metadata.width, parsed.metadata.height, limits)?;
    let mut rgba = allocate_zeroed(rgba_len, budget)?;
    unfilter_and_convert(&mut inflated, &mut rgba, &parsed.metadata, budget)?;
    budget.end_allocation(inflated.len());
    budget.check_now()?;
    let has_alpha = matches!(parsed.metadata.color, ColorType::GrayscaleAlpha | ColorType::Rgba)
        || parsed.metadata.palette_alpha_len > 0
        || parsed.metadata.transparent_gray.is_some()
        || parsed.metadata.transparent_rgb.is_some();
    Ok(DecodedPng { width: parsed.metadata.width, height: parsed.metadata.height, rgba, has_alpha })
}

fn parse_png(
    data: &[u8],
    limits: PngLimits,
    budget: &mut DecodeBudget<'_>,
) -> Result<Parsed, PngError> {
    if data.get(..SIGNATURE.len()) != Some(SIGNATURE) {
        return Err(PngError::InvalidSignature);
    }
    budget.charge(SIGNATURE.len() as u64)?;
    let mut metadata = Metadata::default();
    let mut cursor = SIGNATURE.len();
    let mut saw_ihdr = false;
    let mut saw_idat = false;
    let mut left_idat = false;
    let mut saw_iend = false;
    while cursor < data.len() {
        let (kind, payload, following) = chunk_at(data, cursor, budget)?;
        if !saw_ihdr && kind != *b"IHDR" {
            return Err(PngError::InvalidChunk);
        }
        match &kind {
            b"IHDR" => {
                if saw_ihdr || payload.len() != 13 {
                    return Err(PngError::InvalidChunk);
                }
                parse_ihdr(payload, &mut metadata, limits)?;
                saw_ihdr = true;
            }
            b"PLTE" => {
                if !saw_ihdr || saw_idat || payload.is_empty() || payload.len() % 3 != 0 {
                    return Err(PngError::InvalidChunk);
                }
                let count = payload.len() / 3;
                if count > 256 {
                    return Err(PngError::InvalidChunk);
                }
                for (index, color) in payload.chunks_exact(3).enumerate() {
                    metadata.palette[index].copy_from_slice(color);
                }
                metadata.palette_len = count;
            }
            b"tRNS" => {
                if !saw_ihdr || saw_idat {
                    return Err(PngError::InvalidChunk);
                }
                parse_transparency(payload, &mut metadata)?;
            }
            b"IDAT" => {
                if !saw_ihdr || left_idat {
                    return Err(PngError::InvalidChunk);
                }
                saw_idat = true;
            }
            b"IEND" => {
                if !saw_idat || payload.len() != 0 || following != data.len() {
                    return Err(PngError::InvalidChunk);
                }
                saw_iend = true;
            }
            b"acTL" | b"fcTL" | b"fdAT" => return Err(PngError::UnsupportedAnimation),
            _ if kind[0] & 0x20 == 0 => return Err(PngError::InvalidChunk),
            _ => {}
        }
        if saw_idat && kind != *b"IDAT" && kind != *b"IEND" {
            left_idat = true;
        }
        cursor = following;
        if saw_iend {
            break;
        }
    }
    if !saw_ihdr || !saw_idat || !saw_iend {
        return Err(PngError::InvalidChunk);
    }
    if metadata.color == ColorType::Palette && metadata.palette_len == 0 {
        return Err(PngError::InvalidChunk);
    }
    let inflated_len = expected_inflated_len(&metadata)?;
    if inflated_len > limits.max_inflated_bytes {
        return Err(PngError::QuotaExceeded {
            limit: "inflated_bytes",
            requested: inflated_len as u64,
            maximum: limits.max_inflated_bytes as u64,
        });
    }
    Ok(Parsed { metadata, inflated_len })
}

fn chunk_at<'a>(
    data: &'a [u8],
    cursor: usize,
    budget: &mut DecodeBudget<'_>,
) -> Result<([u8; 4], &'a [u8], usize), PngError> {
    let header_end = cursor.checked_add(8).ok_or(PngError::InvalidChunk)?;
    let header = data.get(cursor..header_end).ok_or(PngError::InvalidChunk)?;
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let payload_end = header_end.checked_add(length).ok_or(PngError::InvalidChunk)?;
    let following = payload_end.checked_add(4).ok_or(PngError::InvalidChunk)?;
    let payload = data.get(header_end..payload_end).ok_or(PngError::InvalidChunk)?;
    let crc_bytes = data.get(payload_end..following).ok_or(PngError::InvalidChunk)?;
    let kind = [header[4], header[5], header[6], header[7]];
    budget.charge((following - cursor) as u64)?;
    let expected = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    if crc32(&kind, payload) != expected {
        return Err(PngError::InvalidCrc);
    }
    Ok((kind, payload, following))
}

fn parse_ihdr(payload: &[u8], metadata: &mut Metadata, limits: PngLimits) -> Result<(), PngError> {
    let width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
    let bit_depth = payload[8];
    let color = match payload[9] {
        0 => ColorType::Grayscale,
        2 => ColorType::Rgb,
        3 => ColorType::Palette,
        4 => ColorType::GrayscaleAlpha,
        6 => ColorType::Rgba,
        _ => return Err(PngError::UnsupportedColor),
    };
    let valid_depth = match color {
        ColorType::Grayscale => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        ColorType::Palette => matches!(bit_depth, 1 | 2 | 4 | 8),
        ColorType::Rgb | ColorType::GrayscaleAlpha | ColorType::Rgba => {
            matches!(bit_depth, 8 | 16)
        }
    };
    if !valid_depth || payload[10] != 0 || payload[11] != 0 || payload[12] > 1 {
        return Err(PngError::UnsupportedColor);
    }
    let _ = guard_dimensions(width, height, limits)?;
    metadata.width = width;
    metadata.height = height;
    metadata.bit_depth = bit_depth;
    metadata.color = color;
    metadata.interlaced = payload[12] == 1;
    Ok(())
}

fn parse_transparency(payload: &[u8], metadata: &mut Metadata) -> Result<(), PngError> {
    match metadata.color {
        ColorType::Grayscale if payload.len() == 2 => {
            metadata.transparent_gray = Some(u16::from_be_bytes([payload[0], payload[1]]));
        }
        ColorType::Rgb if payload.len() == 6 => {
            metadata.transparent_rgb = Some([
                u16::from_be_bytes([payload[0], payload[1]]),
                u16::from_be_bytes([payload[2], payload[3]]),
                u16::from_be_bytes([payload[4], payload[5]]),
            ]);
        }
        ColorType::Palette if payload.len() <= metadata.palette_len => {
            metadata.palette_alpha[..payload.len()].copy_from_slice(payload);
            metadata.palette_alpha_len = payload.len();
        }
        _ => return Err(PngError::InvalidChunk),
    }
    Ok(())
}

fn guard_dimensions(width: usize, height: usize, limits: PngLimits) -> Result<usize, PngError> {
    if width == 0
        || height == 0
        || width > limits.max_width_pixels
        || height > limits.max_height_pixels
    {
        return Err(PngError::InvalidDimensions { width, height });
    }
    let pixels = width.checked_mul(height).ok_or(PngError::InvalidDimensions { width, height })?;
    if pixels > limits.max_pixels {
        return Err(PngError::QuotaExceeded {
            limit: "pixels",
            requested: pixels as u64,
            maximum: limits.max_pixels as u64,
        });
    }
    let bytes = pixels.checked_mul(4).ok_or(PngError::InvalidDimensions { width, height })?;
    if bytes > limits.max_rgba_bytes {
        return Err(PngError::QuotaExceeded {
            limit: "canonical_rgba_bytes",
            requested: bytes as u64,
            maximum: limits.max_rgba_bytes as u64,
        });
    }
    Ok(bytes)
}

fn expected_inflated_len(metadata: &Metadata) -> Result<usize, PngError> {
    if !metadata.interlaced {
        return pass_inflated_len(metadata.width, metadata.height, metadata);
    }
    let mut total = 0usize;
    for (x, y, dx, dy) in ADAM7 {
        let width = span(metadata.width, x, dx);
        let height = span(metadata.height, y, dy);
        if width == 0 || height == 0 {
            continue;
        }
        total = total
            .checked_add(pass_inflated_len(width, height, metadata)?)
            .ok_or(PngError::InvalidChunk)?;
    }
    Ok(total)
}

fn pass_inflated_len(width: usize, height: usize, metadata: &Metadata) -> Result<usize, PngError> {
    let stride = row_bytes(width, metadata)?.checked_add(1).ok_or(PngError::InvalidChunk)?;
    height.checked_mul(stride).ok_or(PngError::InvalidChunk)
}

fn row_bytes(width: usize, metadata: &Metadata) -> Result<usize, PngError> {
    let bits = width
        .checked_mul(metadata.color.channels())
        .and_then(|value| value.checked_mul(metadata.bit_depth as usize))
        .ok_or(PngError::InvalidChunk)?;
    Ok(bits.checked_add(7).ok_or(PngError::InvalidChunk)? / 8)
}

fn span(size: usize, start: usize, step: usize) -> usize {
    if size <= start { 0 } else { (size - start + step - 1) / step }
}

fn inflate_idat(
    data: &[u8],
    expected_len: usize,
    budget: &mut DecodeBudget<'_>,
) -> Result<Vec<u8>, PngError> {
    let mut output = allocate_zeroed(expected_len, budget)?;
    let mut decoder = Decompress::new(true);
    let mut cursor = SIGNATURE.len();
    let mut written = 0usize;
    let mut ended = false;
    while cursor < data.len() {
        let (kind, payload, following) = chunk_at(data, cursor, budget)?;
        if kind == *b"IDAT" {
            let mut consumed = 0usize;
            while consumed < payload.len() {
                let before_in = decoder.total_in();
                let before_out = decoder.total_out();
                let mut overflow = [0u8; 1];
                let target =
                    if written < output.len() { &mut output[written..] } else { &mut overflow[..] };
                let status = decoder
                    .decompress(&payload[consumed..], target, FlushDecompress::None)
                    .map_err(|_| PngError::InflateFailed)?;
                let used = (decoder.total_in() - before_in) as usize;
                let produced = (decoder.total_out() - before_out) as usize;
                budget.charge((used + produced) as u64)?;
                if written == output.len() && produced > 0 {
                    return Err(PngError::InflatedLengthMismatch {
                        expected: expected_len,
                        actual: expected_len.saturating_add(produced),
                    });
                }
                consumed = consumed.checked_add(used).ok_or(PngError::InflateFailed)?;
                written = written.checked_add(produced).ok_or(PngError::InflateFailed)?;
                if status == Status::StreamEnd {
                    ended = true;
                    if consumed != payload.len() {
                        return Err(PngError::InflateFailed);
                    }
                    break;
                }
                if used == 0 && produced == 0 {
                    return Err(PngError::InflateFailed);
                }
            }
        }
        cursor = following;
        if kind == *b"IEND" {
            break;
        }
    }
    if !ended {
        let mut overflow = [0u8; 1];
        let target =
            if written < output.len() { &mut output[written..] } else { &mut overflow[..] };
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(&[], target, FlushDecompress::Finish)
            .map_err(|_| PngError::InflateFailed)?;
        let produced = (decoder.total_out() - before_out) as usize;
        budget.charge(produced as u64)?;
        if written == output.len() && produced > 0 {
            return Err(PngError::InflatedLengthMismatch {
                expected: expected_len,
                actual: expected_len.saturating_add(produced),
            });
        }
        written = written.checked_add(produced).ok_or(PngError::InflateFailed)?;
        ended = status == Status::StreamEnd;
    }
    if !ended || written != expected_len {
        return Err(PngError::InflatedLengthMismatch { expected: expected_len, actual: written });
    }
    Ok(output)
}

fn allocate_zeroed(len: usize, budget: &mut DecodeBudget<'_>) -> Result<Vec<u8>, PngError> {
    budget.begin_allocation(len)?;
    let mut output = Vec::new();
    if output.try_reserve_exact(len).is_err() {
        budget.end_allocation(len);
        return Err(PngError::AllocationFailed { requested_bytes: len });
    }
    output.resize(len, 0);
    Ok(output)
}

fn unfilter_and_convert(
    inflated: &mut [u8],
    rgba: &mut [u8],
    metadata: &Metadata,
    budget: &mut DecodeBudget<'_>,
) -> Result<(), PngError> {
    let filter_bpp = ((metadata.color.channels() * metadata.bit_depth as usize) + 7) / 8;
    let mut offset = 0usize;
    if metadata.interlaced {
        for (start_x, start_y, step_x, step_y) in ADAM7 {
            process_pass(
                inflated,
                rgba,
                metadata,
                budget,
                filter_bpp,
                &mut offset,
                Pass {
                    width: span(metadata.width, start_x, step_x),
                    height: span(metadata.height, start_y, step_y),
                    start_x,
                    start_y,
                    step_x,
                    step_y,
                },
            )?;
        }
    } else {
        process_pass(
            inflated,
            rgba,
            metadata,
            budget,
            filter_bpp,
            &mut offset,
            Pass {
                width: metadata.width,
                height: metadata.height,
                start_x: 0,
                start_y: 0,
                step_x: 1,
                step_y: 1,
            },
        )?;
    }
    if offset != inflated.len() {
        return Err(PngError::InvalidChunk);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Pass {
    width: usize,
    height: usize,
    start_x: usize,
    start_y: usize,
    step_x: usize,
    step_y: usize,
}

fn process_pass(
    inflated: &mut [u8],
    rgba: &mut [u8],
    metadata: &Metadata,
    budget: &mut DecodeBudget<'_>,
    filter_bpp: usize,
    offset: &mut usize,
    pass: Pass,
) -> Result<(), PngError> {
    if pass.width == 0 || pass.height == 0 {
        return Ok(());
    }
    let row_len = row_bytes(pass.width, metadata)?;
    let stride = row_len.checked_add(1).ok_or(PngError::InvalidChunk)?;
    for row in 0..pass.height {
        let row_start = (*offset)
            .checked_add(row.checked_mul(stride).ok_or(PngError::InvalidChunk)?)
            .ok_or(PngError::InvalidChunk)?;
        let data_start = row_start.checked_add(1).ok_or(PngError::InvalidChunk)?;
        let data_end = data_start.checked_add(row_len).ok_or(PngError::InvalidChunk)?;
        let filter = *inflated.get(row_start).ok_or(PngError::InvalidChunk)?;
        for index in 0..row_len {
            let at = data_start + index;
            let raw = inflated[at];
            let left = if index >= filter_bpp { inflated[at - filter_bpp] } else { 0 };
            let above = if row > 0 { inflated[at - stride] } else { 0 };
            let upper_left =
                if row > 0 && index >= filter_bpp { inflated[at - stride - filter_bpp] } else { 0 };
            inflated[at] = match filter {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(above),
                3 => raw.wrapping_add(((u16::from(left) + u16::from(above)) / 2) as u8),
                4 => raw.wrapping_add(paeth(left, above, upper_left)),
                _ => return Err(PngError::InvalidChunk),
            };
        }
        budget.charge(row_len as u64)?;
        let scanline = &inflated[data_start..data_end];
        for column in 0..pass.width {
            let x = pass.start_x + column * pass.step_x;
            let y = pass.start_y + row * pass.step_y;
            let pixel = decode_pixel(scanline, column, metadata)?;
            let target = (y * metadata.width + x) * 4;
            rgba[target..target + 4].copy_from_slice(&pixel);
            budget.charge(1)?;
        }
    }
    *offset = (*offset)
        .checked_add(pass.height.checked_mul(stride).ok_or(PngError::InvalidChunk)?)
        .ok_or(PngError::InvalidChunk)?;
    Ok(())
}

fn decode_pixel(scanline: &[u8], pixel: usize, metadata: &Metadata) -> Result<[u8; 4], PngError> {
    match metadata.color {
        ColorType::Grayscale => {
            let raw = sample(scanline, pixel, 0, metadata)?;
            let gray = scale(raw, metadata.bit_depth);
            let alpha = if metadata.transparent_gray == Some(raw) { 0 } else { 255 };
            Ok([gray, gray, gray, alpha])
        }
        ColorType::Rgb => {
            let raw = [
                sample(scanline, pixel, 0, metadata)?,
                sample(scanline, pixel, 1, metadata)?,
                sample(scanline, pixel, 2, metadata)?,
            ];
            let alpha = if metadata.transparent_rgb == Some(raw) { 0 } else { 255 };
            Ok([
                scale(raw[0], metadata.bit_depth),
                scale(raw[1], metadata.bit_depth),
                scale(raw[2], metadata.bit_depth),
                alpha,
            ])
        }
        ColorType::Palette => {
            let index = sample(scanline, pixel, 0, metadata)? as usize;
            if index >= metadata.palette_len {
                return Err(PngError::InvalidChunk);
            }
            let color = metadata.palette[index];
            Ok([color[0], color[1], color[2], metadata.palette_alpha[index]])
        }
        ColorType::GrayscaleAlpha => {
            let gray = sample(scanline, pixel, 0, metadata)?;
            let alpha = sample(scanline, pixel, 1, metadata)?;
            let value = scale(gray, metadata.bit_depth);
            Ok([value, value, value, scale(alpha, metadata.bit_depth)])
        }
        ColorType::Rgba => Ok([
            scale(sample(scanline, pixel, 0, metadata)?, metadata.bit_depth),
            scale(sample(scanline, pixel, 1, metadata)?, metadata.bit_depth),
            scale(sample(scanline, pixel, 2, metadata)?, metadata.bit_depth),
            scale(sample(scanline, pixel, 3, metadata)?, metadata.bit_depth),
        ]),
    }
}

fn sample(
    scanline: &[u8],
    pixel: usize,
    channel: usize,
    metadata: &Metadata,
) -> Result<u16, PngError> {
    let channels = metadata.color.channels();
    let sample_index = pixel
        .checked_mul(channels)
        .and_then(|value| value.checked_add(channel))
        .ok_or(PngError::InvalidChunk)?;
    match metadata.bit_depth {
        1 | 2 | 4 => {
            let depth = metadata.bit_depth as usize;
            let bit = sample_index * depth;
            let byte = *scanline.get(bit / 8).ok_or(PngError::InvalidChunk)?;
            let shift = 8 - depth - (bit % 8);
            Ok(u16::from((byte >> shift) & ((1u8 << depth) - 1)))
        }
        8 => Ok(u16::from(*scanline.get(sample_index).ok_or(PngError::InvalidChunk)?)),
        16 => {
            let at = sample_index.checked_mul(2).ok_or(PngError::InvalidChunk)?;
            Ok(u16::from_be_bytes([
                *scanline.get(at).ok_or(PngError::InvalidChunk)?,
                *scanline.get(at + 1).ok_or(PngError::InvalidChunk)?,
            ]))
        }
        _ => Err(PngError::UnsupportedColor),
    }
}

fn scale(sample: u16, depth: u8) -> u8 {
    match depth {
        1 => (sample * 255) as u8,
        2 => (sample * 85) as u8,
        4 => (sample * 17) as u8,
        8 => sample as u8,
        16 => (sample >> 8) as u8,
        _ => 0,
    }
}

fn paeth(left: u8, above: u8, upper_left: u8) -> u8 {
    let left = i32::from(left);
    let above = i32::from(above);
    let upper_left = i32::from(upper_left);
    let prediction = left + above - upper_left;
    let left_distance = (prediction - left).abs();
    let above_distance = (prediction - above).abs();
    let upper_left_distance = (prediction - upper_left).abs();
    if left_distance <= above_distance && left_distance <= upper_left_distance {
        left as u8
    } else if above_distance <= upper_left_distance {
        above as u8
    } else {
        upper_left as u8
    }
}

fn crc32(kind: &[u8; 4], payload: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in kind.iter().chain(payload) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}
