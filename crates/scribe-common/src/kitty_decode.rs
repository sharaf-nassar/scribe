//! Strict direct-only Kitty payload normalization.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::{Decompress, FlushDecompress, Status};
use scribe_image_decode::{BudgetError, DecodeBudget, DecodeStats};
use scribe_png_decoder::{PngError, PngLimits, decode_png};

use crate::terminal_images::{ImageLimitName, ImageLimits, TerminalImageRejectionReason};

/// Kitty payload formats accepted by terminal-images v1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyFormat {
    Rgb,
    Rgba,
    Png,
}

/// Source transport classification without carrying a path or resource name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyTransport {
    Direct,
    File,
    TemporaryFile,
    SharedMemory,
    OtherIndirect,
}

/// Optional outer compression from Kitty's `o` control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KittyCompression {
    None,
    Rfc1950Zlib,
}

/// Immutable first-chunk controls for one direct transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KittyDataParams {
    pub format: KittyFormat,
    pub transport: KittyTransport,
    pub compression: KittyCompression,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Payload-free normalization failure suitable for typed diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KittyDecodeError {
    pub reason: TerminalImageRejectionReason,
    pub observed: Option<u64>,
    pub limit: Option<ImageLimitName>,
}

impl KittyDecodeError {
    const fn reason(reason: TerminalImageRejectionReason) -> Self {
        Self { reason, observed: None, limit: None }
    }

    const fn limit(
        reason: TerminalImageRejectionReason,
        observed: u64,
        limit: ImageLimitName,
    ) -> Self {
        Self { reason, observed: Some(observed), limit: Some(limit) }
    }
}

impl std::fmt::Display for KittyDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Kitty decode rejected: {:?}", self.reason)
    }
}

impl std::error::Error for KittyDecodeError {}

impl From<BudgetError> for KittyDecodeError {
    fn from(error: BudgetError) -> Self {
        let reason = match error {
            BudgetError::InvalidLimits | BudgetError::WorkBudgetExceeded { .. } => {
                TerminalImageRejectionReason::WorkBudgetExceeded
            }
            BudgetError::DecodeDeadlineExceeded { .. } => {
                TerminalImageRejectionReason::DecodeDeadlineExceeded
            }
            BudgetError::DecodeCancelled { .. } => TerminalImageRejectionReason::DecodeCancelled,
            BudgetError::AllocationFailed { .. } => TerminalImageRejectionReason::QuotaExceeded,
        };
        Self::reason(reason)
    }
}

/// Completed canonical image. No encoded or compressed payload is retained.
#[derive(Debug)]
pub struct NormalizedKittyImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub has_alpha: bool,
    pub encoded_bytes: u64,
    pub decoded_bytes: u64,
    pub inflated_bytes: u64,
    pub stats: DecodeStats,
}

/// One bounded in-flight Kitty transfer, storing decoded bytes only.
// @lat: [[terminal-images#Terminal Images#Bounded Kitty Normalization]]
pub struct KittyTransfer {
    params: KittyDataParams,
    limits: ImageLimits,
    decoded: Vec<u8>,
    encoded_bytes: u64,
    chunks: u32,
    final_received: bool,
}

struct ChunkPlan {
    payload_len: u64,
    accumulated: u64,
    decoded_len: usize,
    projected: usize,
}

struct DecodedParts {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    has_alpha: bool,
}

impl KittyTransfer {
    /// Reject policy and raw dimensions before payload storage or allocation.
    pub fn new(params: KittyDataParams, limits: ImageLimits) -> Result<Self, KittyDecodeError> {
        if params.transport != KittyTransport::Direct {
            return Err(KittyDecodeError::reason(
                TerminalImageRejectionReason::UnsupportedTransport,
            ));
        }
        if matches!(params.format, KittyFormat::Rgb | KittyFormat::Rgba) {
            let width = params.width.ok_or_else(|| {
                KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
            })?;
            let height = params.height.ok_or_else(|| {
                KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
            })?;
            limits.canonical_rgba_len(width, height).map_err(|_| {
                KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
            })?;
            let channels = if params.format == KittyFormat::Rgb { 3u64 } else { 4u64 };
            let raw_len = u64::from(width)
                .checked_mul(u64::from(height))
                .and_then(|pixels| pixels.checked_mul(channels))
                .ok_or_else(|| {
                    KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
                })?;
            if raw_len > limits.max_inflated_bytes {
                return Err(KittyDecodeError::limit(
                    TerminalImageRejectionReason::QuotaExceeded,
                    raw_len,
                    ImageLimitName::InflatedBytes,
                ));
            }
        }
        Ok(Self {
            params,
            limits,
            decoded: Vec::new(),
            encoded_bytes: 0,
            chunks: 0,
            final_received: false,
        })
    }

    /// Decode one exact RFC 4648 chunk. Non-final chunks must end on a quartet.
    pub fn push_chunk(
        &mut self,
        payload: &[u8],
        more: bool,
        budget: &mut DecodeBudget<'_>,
    ) -> Result<(), KittyDecodeError> {
        let plan = self.plan_chunk(payload, more)?;
        budget.charge(plan.payload_len)?;
        budget.begin_allocation(plan.decoded_len)?;
        if self.decoded.try_reserve_exact(plan.decoded_len).is_err() {
            budget.end_allocation(plan.decoded_len);
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::QuotaExceeded));
        }
        let previous = self.decoded.len();
        self.decoded.resize(plan.projected, 0);
        let output = self
            .decoded
            .get_mut(previous..)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        match STANDARD.decode_slice(payload, output) {
            Ok(written) if written == plan.decoded_len => {}
            _ => {
                self.decoded.truncate(previous);
                budget.end_allocation(plan.decoded_len);
                return Err(KittyDecodeError::reason(
                    TerminalImageRejectionReason::MalformedPayload,
                ));
            }
        }
        budget.charge(plan.decoded_len as u64)?;
        self.encoded_bytes = plan.accumulated;
        self.final_received = !more;
        Ok(())
    }

    fn plan_chunk(&mut self, payload: &[u8], more: bool) -> Result<ChunkPlan, KittyDecodeError> {
        if self.final_received {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::ChunkMismatch));
        }
        let payload_len = payload.len() as u64;
        if payload_len > self.limits.max_kitty_chunk_payload_bytes {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                payload_len,
                ImageLimitName::KittyChunkPayloadBytes,
            ));
        }
        self.chunks = self.chunks.checked_add(1).ok_or_else(|| {
            KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::MAX,
                ImageLimitName::ChunksPerTransfer,
            )
        })?;
        if self.chunks > self.limits.max_chunks_per_transfer {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::from(self.chunks),
                ImageLimitName::ChunksPerTransfer,
            ));
        }
        if !payload.len().is_multiple_of(4) {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::MalformedPayload));
        }
        if more && payload.last() == Some(&b'=') {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::ChunkMismatch));
        }
        let accumulated = self.encoded_bytes.checked_add(payload_len).ok_or_else(|| {
            KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::MAX,
                ImageLimitName::AccumulatedEncodedBytes,
            )
        })?;
        if accumulated > self.limits.max_accumulated_encoded_bytes {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                accumulated,
                ImageLimitName::AccumulatedEncodedBytes,
            ));
        }
        let padding = payload.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::MalformedPayload));
        }
        let decoded_len = payload.len() / 4 * 3 - padding;
        let projected = self.decoded.len().checked_add(decoded_len).ok_or_else(|| {
            KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::MAX,
                ImageLimitName::Base64DecodedBytes,
            )
        })?;
        if projected as u64 > self.limits.max_base64_decoded_bytes {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                projected as u64,
                ImageLimitName::Base64DecodedBytes,
            ));
        }
        Ok(ChunkPlan { payload_len, accumulated, decoded_len, projected })
    }

    /// Finish validation and return only canonical RGBA.
    pub fn finish(
        self,
        budget: &mut DecodeBudget<'_>,
    ) -> Result<NormalizedKittyImage, KittyDecodeError> {
        if !self.final_received {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::ChunkMismatch));
        }
        let decoded_bytes = self.decoded.len() as u64;
        let (payload, inflated_bytes) = match self.params.compression {
            KittyCompression::None => (self.decoded, decoded_bytes),
            KittyCompression::Rfc1950Zlib => {
                let maximum =
                    limit_to_usize(self.limits.max_inflated_bytes, ImageLimitName::InflatedBytes)?;
                let inflated = inflate_rfc1950(&self.decoded, maximum, budget)?;
                let len = inflated.len() as u64;
                budget.end_allocation(self.decoded.len());
                (inflated, len)
            }
        };
        let parts = match self.params.format {
            KittyFormat::Rgb | KittyFormat::Rgba => {
                normalize_raw(self.params, &payload, self.limits, budget)?
            }
            KittyFormat::Png => {
                let png_limits = PngLimits {
                    max_width_pixels: self.limits.max_width_pixels as usize,
                    max_height_pixels: self.limits.max_height_pixels as usize,
                    max_pixels: limit_to_usize(self.limits.max_pixels, ImageLimitName::Pixels)?,
                    max_inflated_bytes: limit_to_usize(
                        self.limits.max_inflated_bytes,
                        ImageLimitName::InflatedBytes,
                    )?,
                    max_rgba_bytes: limit_to_usize(
                        self.limits.max_canonical_rgba_bytes,
                        ImageLimitName::CanonicalRgbaBytes,
                    )?,
                };
                let decoded = decode_png(&payload, png_limits, budget).map_err(map_png_error)?;
                let width = u32::try_from(decoded.width).map_err(|_| {
                    KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
                })?;
                let height = u32::try_from(decoded.height).map_err(|_| {
                    KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
                })?;
                DecodedParts { width, height, rgba: decoded.rgba, has_alpha: decoded.has_alpha }
            }
        };
        budget.end_allocation(payload.len());
        budget.check_now()?;
        Ok(NormalizedKittyImage {
            width: parts.width,
            height: parts.height,
            rgba: parts.rgba,
            has_alpha: parts.has_alpha,
            encoded_bytes: self.encoded_bytes,
            decoded_bytes,
            inflated_bytes,
            stats: budget.stats(),
        })
    }
}

fn normalize_raw(
    params: KittyDataParams,
    payload: &[u8],
    limits: ImageLimits,
    budget: &mut DecodeBudget<'_>,
) -> Result<DecodedParts, KittyDecodeError> {
    let width = params
        .width
        .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions))?;
    let height = params
        .height
        .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions))?;
    let rgba_len =
        usize::try_from(limits.canonical_rgba_len(width, height).map_err(|_| {
            KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
        })?)
        .map_err(|_| KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions))?;
    let channels = if params.format == KittyFormat::Rgb { 3usize } else { 4usize };
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels))
        .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions))?;
    if payload.len() != expected {
        return Err(KittyDecodeError {
            reason: TerminalImageRejectionReason::MalformedPayload,
            observed: Some(payload.len() as u64),
            limit: None,
        });
    }
    budget.begin_allocation(rgba_len)?;
    let mut rgba = Vec::new();
    if rgba.try_reserve_exact(rgba_len).is_err() {
        budget.end_allocation(rgba_len);
        return Err(KittyDecodeError::reason(TerminalImageRejectionReason::QuotaExceeded));
    }
    if channels == 4 {
        rgba.extend_from_slice(payload);
        budget.charge(u64::from(width) * u64::from(height))?;
    } else {
        for pixel in payload.chunks_exact(3) {
            rgba.extend_from_slice(pixel);
            rgba.push(255);
            budget.charge(1)?;
        }
    }
    Ok(DecodedParts { width, height, rgba, has_alpha: channels == 4 })
}

fn inflate_rfc1950(
    input: &[u8],
    maximum: usize,
    budget: &mut DecodeBudget<'_>,
) -> Result<Vec<u8>, KittyDecodeError> {
    let mut decoder = Decompress::new(true);
    let mut output = Vec::new();
    let mut consumed = 0usize;
    loop {
        let mut scratch = [0u8; 4_096];
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let flush =
            if consumed == input.len() { FlushDecompress::Finish } else { FlushDecompress::None };
        let remaining = input
            .get(consumed..)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let status = decoder
            .decompress(remaining, &mut scratch, flush)
            .map_err(|_| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let used = usize::try_from(decoder.total_in().saturating_sub(before_in))
            .map_err(|_| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let produced = usize::try_from(decoder.total_out().saturating_sub(before_out))
            .map_err(|_| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        budget.charge((used + produced) as u64)?;
        consumed = consumed
            .checked_add(used)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let projected = output.len().checked_add(produced).ok_or_else(|| {
            KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::MAX,
                ImageLimitName::InflatedBytes,
            )
        })?;
        if projected > maximum {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                projected as u64,
                ImageLimitName::InflatedBytes,
            ));
        }
        budget.begin_allocation(produced)?;
        if output.try_reserve_exact(produced).is_err() {
            budget.end_allocation(produced);
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::QuotaExceeded));
        }
        let produced_bytes = scratch
            .get(..produced)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        output.extend_from_slice(produced_bytes);
        if status == Status::StreamEnd {
            if consumed != input.len() {
                return Err(KittyDecodeError::reason(
                    TerminalImageRejectionReason::MalformedPayload,
                ));
            }
            return Ok(output);
        }
        if used == 0 && produced == 0 {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed));
        }
    }
}

fn limit_to_usize(value: u64, limit: ImageLimitName) -> Result<usize, KittyDecodeError> {
    usize::try_from(value).map_err(|_| {
        KittyDecodeError::limit(TerminalImageRejectionReason::QuotaExceeded, value, limit)
    })
}

fn map_png_error(error: PngError) -> KittyDecodeError {
    let reason = match error {
        PngError::InvalidDimensions { .. } => TerminalImageRejectionReason::InvalidDimensions,
        PngError::QuotaExceeded { .. } | PngError::AllocationFailed { .. } => {
            TerminalImageRejectionReason::QuotaExceeded
        }
        PngError::WorkBudgetExceeded => TerminalImageRejectionReason::WorkBudgetExceeded,
        PngError::DecodeDeadlineExceeded => TerminalImageRejectionReason::DecodeDeadlineExceeded,
        PngError::DecodeCancelled => TerminalImageRejectionReason::DecodeCancelled,
        PngError::InflateFailed | PngError::InflatedLengthMismatch { .. } => {
            TerminalImageRejectionReason::DecodeFailed
        }
        PngError::InvalidSignature
        | PngError::InvalidChunk
        | PngError::InvalidCrc
        | PngError::UnsupportedAnimation
        | PngError::UnsupportedColor => TerminalImageRejectionReason::MalformedPayload,
    };
    KittyDecodeError::reason(reason)
}
