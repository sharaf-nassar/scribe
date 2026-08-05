//! Strict direct-only Kitty payload normalization.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::{Decompress, FlushDecompress, Status};
use scribe_image_decode::{
    BudgetError, DecodeAllocationClass, DecodeBudget, DecodeBuffer, DecodeStats, DecodeStorageError,
};
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
    pub storage: Option<DecodeStorageError>,
}

impl KittyDecodeError {
    const fn reason(reason: TerminalImageRejectionReason) -> Self {
        Self { reason, observed: None, limit: None, storage: None }
    }

    const fn limit(
        reason: TerminalImageRejectionReason,
        observed: u64,
        limit: ImageLimitName,
    ) -> Self {
        Self { reason, observed: Some(observed), limit: Some(limit), storage: None }
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
            BudgetError::Storage(storage) => {
                return Self {
                    reason: TerminalImageRejectionReason::QuotaExceeded,
                    observed: None,
                    limit: None,
                    storage: Some(storage),
                };
            }
        };
        Self::reason(reason)
    }
}

/// Completed canonical image. No encoded or compressed payload is retained.
#[derive(Debug)]
pub struct NormalizedKittyImage {
    pub width: u32,
    pub height: u32,
    pub rgba: DecodeBuffer,
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
    decoded: Option<DecodeBuffer>,
    encoded_bytes: u64,
    chunks: u32,
    final_received: bool,
    transaction: Option<KittyTransferTransaction>,
}

struct KittyTransferTransaction {
    decoded_len: usize,
    encoded_bytes: u64,
    chunks: u32,
    final_received: bool,
    original_decoded: Option<DecodeBuffer>,
}

struct ChunkPlan {
    payload_len: u64,
    accumulated: u64,
    decoded_len: usize,
    projected: usize,
    chunks: u32,
}

struct DecodedParts {
    width: u32,
    height: u32,
    rgba: DecodeBuffer,
    has_alpha: bool,
}

fn decode_chunk(
    payload: &[u8],
    previous: usize,
    decoded_len: usize,
    decoded: &mut DecodeBuffer,
) -> Result<(), KittyDecodeError> {
    let output = decoded
        .get_mut(previous..)
        .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
    match STANDARD.decode_slice(payload, output) {
        Ok(written) if written == decoded_len => Ok(()),
        _ => Err(KittyDecodeError::reason(TerminalImageRejectionReason::MalformedPayload)),
    }
}

impl KittyTransfer {
    #[must_use]
    pub fn retained_requested_bytes(&self) -> usize {
        self.decoded.as_ref().map_or(0, DecodeBuffer::requested_bytes)
    }

    #[must_use]
    pub fn retained_observed_bytes(&self) -> usize {
        self.decoded.as_ref().map_or(0, DecodeBuffer::observed_bytes)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn validation_digest(&self) -> u64 {
        self.decoded.as_ref().map_or(0, |decoded| {
            decoded.iter().fold(0, |digest, byte| {
                digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
            })
        })
    }

    #[doc(hidden)]
    #[must_use]
    pub fn validation_state(&self) -> (u32, usize, bool) {
        (self.chunks, self.decoded.as_ref().map_or(0, |decoded| decoded.len()), self.final_received)
    }

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
            decoded: None,
            encoded_bytes: 0,
            chunks: 0,
            final_received: false,
            transaction: None,
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
        // Admission gates the work: this chunk's encoded input and the bytes
        // its decode will write are charged before either is touched.
        budget.charge(plan.payload_len)?;
        budget.charge(plan.decoded_len as u64)?;
        let previous = self.decoded.as_ref().map_or(0, |decoded| decoded.len());
        let current_capacity = self.decoded.as_ref().map_or(0, DecodeBuffer::capacity);
        if plan.projected > current_capacity {
            let requested = current_capacity
                .max(1)
                .checked_mul(2)
                .unwrap_or(plan.projected)
                .max(plan.projected);
            budget.charge(previous as u64)?;
            let mut replacement = budget.allocate(DecodeAllocationClass::KittyBase64, requested)?;
            if let Some(decoded) = &self.decoded {
                replacement
                    .extend_from_slice(decoded)
                    .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            }
            replacement
                .resize(plan.projected, 0)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            decode_chunk(payload, previous, plan.decoded_len, &mut replacement)?;
            let old = self.decoded.replace(replacement);
            self.retain_transaction_owner(old, budget);
        } else {
            let decoded = self.decoded.as_mut().ok_or_else(|| {
                KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed)
            })?;
            decoded
                .resize(plan.projected, 0)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            if let Err(error) = decode_chunk(payload, previous, plan.decoded_len, decoded) {
                decoded.truncate(previous);
                return Err(error);
            }
        }
        self.encoded_bytes = plan.accumulated;
        self.chunks = plan.chunks;
        self.final_received = !more;
        Ok(())
    }

    /// Begin one outer `SessionTerminal` transaction without copying in-capacity bytes.
    pub fn begin_transaction(&mut self) -> Result<(), KittyDecodeError> {
        if self.transaction.is_some() {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed));
        }
        self.transaction = Some(KittyTransferTransaction {
            decoded_len: self.decoded.as_ref().map_or(0, |decoded| decoded.len()),
            encoded_bytes: self.encoded_bytes,
            chunks: self.chunks,
            final_received: self.final_received,
            original_decoded: None,
        });
        Ok(())
    }

    #[must_use]
    pub fn transaction_active(&self) -> bool {
        self.transaction.is_some()
    }

    fn retain_transaction_owner(
        &mut self,
        old: Option<DecodeBuffer>,
        budget: &mut DecodeBudget<'_>,
    ) {
        let old_requested = old.as_ref().map_or(0, DecodeBuffer::requested_bytes);
        let Some(transaction) = self.transaction.as_mut() else {
            budget.end_allocation(old_requested);
            return;
        };
        if transaction.original_decoded.is_none() {
            transaction.original_decoded = old;
            return;
        }
        budget.end_allocation(old_requested);
    }

    /// Commit staged chunks and release any superseded pre-growth owner.
    pub fn commit_transaction(&mut self) -> Result<(), KittyDecodeError> {
        if self.transaction.take().is_none() {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed));
        }
        Ok(())
    }

    /// Restore exact chunk counters, length, and pre-growth owner.
    pub fn rollback_transaction(&mut self) -> Result<(), KittyDecodeError> {
        let transaction = self
            .transaction
            .take()
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        if let Some(original) = transaction.original_decoded {
            self.decoded = Some(original);
        } else if let Some(decoded) = self.decoded.as_mut() {
            if transaction.decoded_len > decoded.len() {
                return Err(KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed));
            }
            decoded.truncate(transaction.decoded_len);
        }
        self.encoded_bytes = transaction.encoded_bytes;
        self.chunks = transaction.chunks;
        self.final_received = transaction.final_received;
        Ok(())
    }

    /// Finalize through a one-time leased copy while retaining this transfer for rollback.
    pub fn finish_preserving(
        &self,
        budget: &mut DecodeBudget<'_>,
    ) -> Result<NormalizedKittyImage, KittyDecodeError> {
        let decoded = if let Some(current) = &self.decoded {
            budget.charge(current.len() as u64)?;
            let mut copy = budget.allocate(
                DecodeAllocationClass::KittyBase64,
                current.capacity().max(current.len()),
            )?;
            copy.extend_from_slice(current)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            Some(copy)
        } else {
            None
        };
        Self {
            params: self.params,
            limits: self.limits,
            decoded,
            encoded_bytes: self.encoded_bytes,
            chunks: self.chunks,
            final_received: self.final_received,
            transaction: None,
        }
        .finish(budget)
    }

    fn plan_chunk(&self, payload: &[u8], more: bool) -> Result<ChunkPlan, KittyDecodeError> {
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
        let chunks = self.chunks.checked_add(1).ok_or_else(|| {
            KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::MAX,
                ImageLimitName::ChunksPerTransfer,
            )
        })?;
        if chunks > self.limits.max_chunks_per_transfer {
            return Err(KittyDecodeError::limit(
                TerminalImageRejectionReason::QuotaExceeded,
                u64::from(chunks),
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
        let projected = self
            .decoded
            .as_ref()
            .map_or(0, |decoded| decoded.len())
            .checked_add(decoded_len)
            .ok_or_else(|| {
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
        Ok(ChunkPlan { payload_len, accumulated, decoded_len, projected, chunks })
    }

    /// Finish validation and return only canonical RGBA.
    pub fn finish(
        self,
        budget: &mut DecodeBudget<'_>,
    ) -> Result<NormalizedKittyImage, KittyDecodeError> {
        if !self.final_received {
            return Err(KittyDecodeError::reason(TerminalImageRejectionReason::ChunkMismatch));
        }
        let decoded = self.decoded.ok_or_else(|| {
            KittyDecodeError::reason(TerminalImageRejectionReason::MalformedPayload)
        })?;
        let decoded_bytes = decoded.len() as u64;
        let (payload, inflated_bytes) = match self.params.compression {
            KittyCompression::None => (decoded, decoded_bytes),
            KittyCompression::Rfc1950Zlib => {
                let maximum =
                    limit_to_usize(self.limits.max_inflated_bytes, ImageLimitName::InflatedBytes)?;
                let inflated = inflate_rfc1950(&decoded, maximum, budget)?;
                let len = inflated.len() as u64;
                budget.end_allocation(decoded.requested_bytes());
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
                let png_decoded =
                    decode_png(&payload, png_limits, budget).map_err(map_png_error)?;
                let width = u32::try_from(png_decoded.width).map_err(|_| {
                    KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
                })?;
                let height = u32::try_from(png_decoded.height).map_err(|_| {
                    KittyDecodeError::reason(TerminalImageRejectionReason::InvalidDimensions)
                })?;
                DecodedParts {
                    width,
                    height,
                    rgba: png_decoded.rgba,
                    has_alpha: png_decoded.has_alpha,
                }
            }
        };
        budget.end_allocation(payload.requested_bytes());
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
            storage: None,
        });
    }
    // Admission gates the work: the copy is charged before the buffer that
    // receives it is even reserved.
    if channels == 4 {
        budget.charge(u64::from(width) * u64::from(height))?;
    }
    let mut rgba = budget.allocate(DecodeAllocationClass::KittyRgba, rgba_len)?;
    if channels == 4 {
        rgba.extend_from_slice(payload)
            .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
    } else {
        for pixel in payload.chunks_exact(3) {
            budget.charge(1)?;
            rgba.extend_from_slice(pixel)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            rgba.push(255).map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
        }
    }
    Ok(DecodedParts { width, height, rgba, has_alpha: channels == 4 })
}

fn inflate_rfc1950(
    input: &[u8],
    maximum: usize,
    budget: &mut DecodeBudget<'_>,
) -> Result<DecodeBuffer, KittyDecodeError> {
    let mut decoder = Decompress::new(true);
    let mut output: Option<DecodeBuffer> = None;
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
        consumed = consumed
            .checked_add(used)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let output_len = output.as_ref().map_or(0, |buffer| buffer.len());
        let projected = output_len.checked_add(produced).ok_or_else(|| {
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
        // `push_chunk` already charges every encoded byte and its decoded
        // compressed byte. Inflation owns produced-output and geometric-copy
        // work; charging `used` here would count compressed input twice and
        // let the work ceiling mask the max+1 inflated-byte quota boundary.
        budget.charge(produced as u64)?;
        let produced_bytes = scratch
            .get(..produced)
            .ok_or_else(|| KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed))?;
        let capacity = output.as_ref().map_or(0, DecodeBuffer::capacity);
        if projected > capacity {
            let requested =
                capacity.max(1).checked_mul(2).unwrap_or(projected).min(maximum).max(projected);
            budget.charge(output_len as u64)?;
            let mut replacement =
                budget.allocate(DecodeAllocationClass::KittyInflate, requested)?;
            if let Some(existing) = &output {
                replacement
                    .extend_from_slice(existing)
                    .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            }
            replacement
                .extend_from_slice(produced_bytes)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
            let old_requested = output.as_ref().map_or(0, DecodeBuffer::requested_bytes);
            output = Some(replacement);
            budget.end_allocation(old_requested);
        } else if let Some(existing) = output.as_mut() {
            existing
                .extend_from_slice(produced_bytes)
                .map_err(|error| KittyDecodeError::from(BudgetError::Storage(error)))?;
        }
        if status == Status::StreamEnd {
            if consumed != input.len() {
                return Err(KittyDecodeError::reason(
                    TerminalImageRejectionReason::MalformedPayload,
                ));
            }
            return output.ok_or_else(|| {
                KittyDecodeError::reason(TerminalImageRejectionReason::DecodeFailed)
            });
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
        PngError::Storage(storage) => {
            return KittyDecodeError {
                reason: TerminalImageRejectionReason::QuotaExceeded,
                observed: None,
                limit: None,
                storage: Some(storage),
            };
        }
    };
    KittyDecodeError::reason(reason)
}
