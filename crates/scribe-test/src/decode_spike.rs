//! Executable bounded-decoder feasibility probe for terminal-image E2E evidence.
//!
//! This is intentionally harness-only. Production decoders are implemented by
//! the dependent terminal-image tasks after this spike's library boundaries are
//! reviewed.

use std::fs;
use std::io::{BufReader, Cursor, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Deserialize, Serialize)]
struct Contract {
    contract_version: String,
    limits: Limits,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct Limits {
    max_base64_decoded_bytes: u64,
    max_inflated_bytes: u64,
    max_width_pixels: u32,
    max_height_pixels: u32,
    max_pixels: u64,
    max_canonical_rgba_bytes: u64,
    max_work_units_per_command: u64,
    max_decode_ms: u64,
    deadline_check_interval_work_units: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rejection {
    AllocationFailed,
    DecodeCancelled,
    DecodeDeadlineExceeded,
    DecodeFailed,
    InvalidDimensions,
    QuotaExceeded,
    WorkBudgetExceeded,
}

impl Rejection {
    const fn category(self) -> &'static str {
        match self {
            Self::AllocationFailed => "allocation_failed",
            Self::DecodeCancelled => "decode_cancelled",
            Self::DecodeDeadlineExceeded => "decode_deadline_exceeded",
            Self::DecodeFailed => "decode_failed",
            Self::InvalidDimensions => "invalid_dimensions",
            Self::QuotaExceeded => "quota_exceeded",
            Self::WorkBudgetExceeded => "work_budget_exceeded",
        }
    }
}

#[derive(Debug)]
struct WorkGuard<'a> {
    cancel: &'a AtomicBool,
    deadline: Duration,
    interval: u64,
    limit: u64,
    next_check: u64,
    started: Instant,
    work: u64,
    checks: u64,
}

impl<'a> WorkGuard<'a> {
    fn new(limits: &Limits, cancel: &'a AtomicBool) -> Self {
        Self::with_deadline(limits, cancel, Duration::from_millis(limits.max_decode_ms))
    }

    fn with_deadline(limits: &Limits, cancel: &'a AtomicBool, deadline: Duration) -> Self {
        Self {
            cancel,
            deadline,
            interval: limits.deadline_check_interval_work_units,
            limit: limits.max_work_units_per_command,
            next_check: limits.deadline_check_interval_work_units,
            started: Instant::now(),
            work: 0,
            checks: 0,
        }
    }

    fn charge(&mut self, units: u64) -> Result<(), Rejection> {
        let projected = self.work.checked_add(units).ok_or(Rejection::WorkBudgetExceeded)?;
        if projected > self.limit {
            return Err(Rejection::WorkBudgetExceeded);
        }
        while self.next_check <= projected {
            self.work = self.next_check;
            self.check()?;
            self.next_check =
                self.next_check.checked_add(self.interval).ok_or(Rejection::WorkBudgetExceeded)?;
        }
        self.work = projected;
        Ok(())
    }

    fn check(&mut self) -> Result<(), Rejection> {
        self.checks = self.checks.saturating_add(1);
        if self.cancel.load(Ordering::Acquire) {
            return Err(Rejection::DecodeCancelled);
        }
        if self.started.elapsed() >= self.deadline {
            return Err(Rejection::DecodeDeadlineExceeded);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AllocationGate {
    ceiling: usize,
    peak: usize,
}

impl AllocationGate {
    const fn new(ceiling: usize) -> Self {
        Self { ceiling, peak: 0 }
    }

    fn resize(&mut self, bytes: &mut Vec<u8>, new_len: usize) -> Result<(), Rejection> {
        if new_len > self.ceiling {
            return Err(Rejection::AllocationFailed);
        }
        if new_len > bytes.len() {
            bytes
                .try_reserve_exact(new_len - bytes.len())
                .map_err(|_| Rejection::AllocationFailed)?;
            bytes.resize(new_len, 0);
        } else {
            bytes.truncate(new_len);
        }
        self.peak = self.peak.max(new_len);
        Ok(())
    }
}

#[derive(Debug)]
struct InflateResult {
    bytes: Vec<u8>,
    checks: u64,
    work: u64,
}

fn dimensions(limits: &Limits, width: u32, height: u32) -> Result<usize, Rejection> {
    if width == 0
        || height == 0
        || width > limits.max_width_pixels
        || height > limits.max_height_pixels
    {
        return Err(Rejection::InvalidDimensions);
    }
    let pixels =
        u64::from(width).checked_mul(u64::from(height)).ok_or(Rejection::InvalidDimensions)?;
    if pixels > limits.max_pixels {
        return Err(Rejection::InvalidDimensions);
    }
    let bytes = pixels.checked_mul(4).ok_or(Rejection::InvalidDimensions)?;
    if bytes > limits.max_canonical_rgba_bytes {
        return Err(Rejection::InvalidDimensions);
    }
    usize::try_from(bytes).map_err(|_| Rejection::InvalidDimensions)
}

fn bounded_inflate(
    input: &[u8],
    limits: &Limits,
    allocation_ceiling: usize,
    cancel: &AtomicBool,
    deadline: Duration,
) -> Result<InflateResult, Rejection> {
    if u64::try_from(input.len()).map_err(|_| Rejection::QuotaExceeded)?
        > limits.max_base64_decoded_bytes
    {
        return Err(Rejection::QuotaExceeded);
    }

    let mut decoder = Decompress::new(true);
    let mut guard = WorkGuard::with_deadline(limits, cancel, deadline);
    let mut gate = AllocationGate::new(allocation_ceiling);
    let mut output = Vec::new();
    let mut scratch = [0_u8; 4096];
    let mut input_offset = 0_usize;

    loop {
        let input_before = decoder.total_in();
        let output_before = decoder.total_out();
        let flush = if input_offset == input.len() {
            FlushDecompress::Finish
        } else {
            FlushDecompress::None
        };
        let remaining_input = input.get(input_offset..).ok_or(Rejection::DecodeFailed)?;
        let status = decoder
            .decompress(remaining_input, &mut scratch, flush)
            .map_err(|_| Rejection::DecodeFailed)?;
        let consumed = decoder.total_in().saturating_sub(input_before);
        let produced = decoder.total_out().saturating_sub(output_before);
        guard.charge(consumed)?;
        guard.charge(produced)?;
        input_offset = input_offset
            .checked_add(usize::try_from(consumed).map_err(|_| Rejection::QuotaExceeded)?)
            .ok_or(Rejection::QuotaExceeded)?;

        let projected = u64::try_from(output.len())
            .map_err(|_| Rejection::QuotaExceeded)?
            .checked_add(produced)
            .ok_or(Rejection::QuotaExceeded)?;
        if projected > limits.max_inflated_bytes {
            return Err(Rejection::QuotaExceeded);
        }
        let projected_usize = usize::try_from(projected).map_err(|_| Rejection::QuotaExceeded)?;
        gate.resize(&mut output, projected_usize)?;
        let produced_usize = usize::try_from(produced).map_err(|_| Rejection::QuotaExceeded)?;
        let start = projected_usize.saturating_sub(produced_usize);
        let target = output.get_mut(start..projected_usize).ok_or(Rejection::DecodeFailed)?;
        let source = scratch.get(..produced_usize).ok_or(Rejection::DecodeFailed)?;
        target.copy_from_slice(source);

        match status {
            Status::StreamEnd => {
                if input_offset != input.len() {
                    return Err(Rejection::DecodeFailed);
                }
                break;
            }
            Status::Ok | Status::BufError if consumed != 0 || produced != 0 => {}
            Status::Ok | Status::BufError => return Err(Rejection::DecodeFailed),
        }
    }

    Ok(InflateResult { bytes: output, checks: guard.checks, work: guard.work })
}

fn zlib_zeros(size: u64) -> Result<Vec<u8>, String> {
    let mut encoder = Compress::new(Compression::best(), true);
    let input = vec![0_u8; 65_536].into_boxed_slice();
    let mut scratch = [0_u8; 4096];
    let mut remaining = size;
    let mut output = Vec::new();

    while remaining != 0 {
        let amount = usize::try_from(remaining.min(input.len() as u64))
            .map_err(|_| "compression input length overflow".to_owned())?;
        let mut offset = 0_usize;
        while offset < amount {
            let input_before = encoder.total_in();
            let output_before = encoder.total_out();
            let input_chunk = input
                .get(offset..amount)
                .ok_or_else(|| "compression input range is invalid".to_owned())?;
            encoder
                .compress(input_chunk, &mut scratch, FlushCompress::None)
                .map_err(|error| format!("zlib bomb fixture compression failed: {error}"))?;
            let consumed = usize::try_from(encoder.total_in().saturating_sub(input_before))
                .map_err(|_| "compression input counter overflow".to_owned())?;
            let produced = usize::try_from(encoder.total_out().saturating_sub(output_before))
                .map_err(|_| "compression output counter overflow".to_owned())?;
            let produced_bytes = scratch
                .get(..produced)
                .ok_or_else(|| "compression output range is invalid".to_owned())?;
            output.extend_from_slice(produced_bytes);
            if consumed == 0 && produced == 0 {
                return Err("zlib bomb fixture compression made no progress".to_owned());
            }
            offset += consumed;
        }
        remaining -= amount as u64;
    }

    loop {
        let output_before = encoder.total_out();
        let status = encoder
            .compress(&[], &mut scratch, FlushCompress::Finish)
            .map_err(|error| format!("zlib bomb fixture finish failed: {error}"))?;
        let produced = usize::try_from(encoder.total_out().saturating_sub(output_before))
            .map_err(|_| "compression finish counter overflow".to_owned())?;
        let produced_bytes = scratch
            .get(..produced)
            .ok_or_else(|| "compression finish range is invalid".to_owned())?;
        output.extend_from_slice(produced_bytes);
        if status == Status::StreamEnd {
            return Ok(output);
        }
        if produced == 0 {
            return Err("zlib bomb fixture finish made no progress".to_owned());
        }
    }
}

fn crc32(kind: [u8; 4], data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in kind.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320_u32 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn append_png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) -> Result<(), String> {
    let length = u32::try_from(data.len()).map_err(|_| "PNG chunk is too large".to_owned())?;
    png.extend_from_slice(&length.to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    png.extend_from_slice(&crc32(kind, data).to_be_bytes());
    Ok(())
}

fn png_rgba(width: u32, height: u32, idat: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut png = PNG_SIGNATURE.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    append_png_chunk(&mut png, *b"IHDR", &ihdr)?;
    if let Some(idat) = idat {
        append_png_chunk(&mut png, *b"IDAT", idat)?;
        append_png_chunk(&mut png, *b"IEND", &[])?;
    }
    Ok(png)
}

fn png_header_dimensions(bytes: &[u8], limits: &Limits) -> Result<(u32, u32), Rejection> {
    if bytes.len() < 33
        || bytes.get(..8) != Some(PNG_SIGNATURE)
        || bytes.get(12..16) != Some(b"IHDR")
    {
        return Err(Rejection::DecodeFailed);
    }
    let width = u32::from_be_bytes(
        bytes
            .get(16..20)
            .ok_or(Rejection::DecodeFailed)?
            .try_into()
            .map_err(|_| Rejection::DecodeFailed)?,
    );
    let height = u32::from_be_bytes(
        bytes
            .get(20..24)
            .ok_or(Rejection::DecodeFailed)?
            .try_into()
            .map_err(|_| Rejection::DecodeFailed)?,
    );
    dimensions(limits, width, height)?;
    Ok((width, height))
}

fn png_idat(bytes: &[u8], ceiling: usize) -> Result<Vec<u8>, Rejection> {
    if bytes.len() < 8 || bytes.get(..8) != Some(PNG_SIGNATURE) {
        return Err(Rejection::DecodeFailed);
    }
    let mut offset = 8_usize;
    let mut idat = Vec::new();
    let mut gate = AllocationGate::new(ceiling);
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or(Rejection::DecodeFailed)?;
        if header_end > bytes.len() {
            return Err(Rejection::DecodeFailed);
        }
        let length_bytes = bytes.get(offset..offset + 4).ok_or(Rejection::DecodeFailed)?;
        let length = usize::try_from(u32::from_be_bytes(
            length_bytes.try_into().map_err(|_| Rejection::DecodeFailed)?,
        ))
        .map_err(|_| Rejection::DecodeFailed)?;
        let chunk_end = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(4))
            .ok_or(Rejection::DecodeFailed)?;
        if chunk_end > bytes.len() {
            return Err(Rejection::DecodeFailed);
        }
        if bytes.get(offset + 4..offset + 8) == Some(b"IDAT") {
            let new_len = idat.len().checked_add(length).ok_or(Rejection::QuotaExceeded)?;
            gate.resize(&mut idat, new_len)?;
            let target = idat.get_mut(new_len - length..).ok_or(Rejection::DecodeFailed)?;
            let source =
                bytes.get(header_end..header_end + length).ok_or(Rejection::DecodeFailed)?;
            target.copy_from_slice(source);
        }
        offset = chunk_end;
    }
    Ok(idat)
}

fn decode_small_png(bytes: &[u8], limits: &Limits) -> Result<usize, Rejection> {
    let (width, height) = png_header_dimensions(bytes, limits)?;
    let mut decoder = png_spike::Decoder::new(BufReader::new(Cursor::new(bytes)));
    decoder.set_limits(png_spike::Limits {
        bytes: usize::try_from(limits.max_inflated_bytes).map_err(|_| Rejection::QuotaExceeded)?,
    });
    let mut reader = decoder.read_info().map_err(|_| Rejection::DecodeFailed)?;
    if reader.info().width != width || reader.info().height != height {
        return Err(Rejection::DecodeFailed);
    }
    let output_size = reader.output_buffer_size().ok_or(Rejection::QuotaExceeded)?;
    let canonical_size = dimensions(limits, width, height)?;
    if output_size > canonical_size {
        return Err(Rejection::QuotaExceeded);
    }
    let mut gate = AllocationGate::new(canonical_size);
    let mut output = Vec::new();
    gate.resize(&mut output, output_size)?;
    let info = reader.next_frame(&mut output).map_err(|_| Rejection::DecodeFailed)?;
    Ok(info.buffer_size())
}

fn gradual_sixel_growth(limits: &Limits) -> Result<(Rejection, usize, u32), String> {
    let cancel = AtomicBool::new(false);
    let mut guard = WorkGuard::new(limits, &cancel);
    let ceiling = usize::try_from(limits.max_canonical_rgba_bytes)
        .map_err(|_| "canonical ceiling does not fit usize".to_owned())?;
    let mut gate = AllocationGate::new(ceiling);
    let mut pixels = Vec::new();
    for width in 1..=limits.max_width_pixels.saturating_add(1) {
        guard.charge(1).map_err(|error| error.category().to_owned())?;
        match dimensions(limits, width, 1) {
            Ok(bytes) => {
                gate.resize(&mut pixels, bytes).map_err(|error| error.category().to_owned())?;
            }
            Err(error) => return Ok((error, gate.peak, width)),
        }
    }
    Err("gradual Sixel growth did not reach the width ceiling".to_owned())
}

fn expect_rejection<T>(
    result: &Result<T, Rejection>,
    expected: Rejection,
    case: &str,
) -> Result<(), String> {
    match result {
        Err(actual) if *actual == expected => Ok(()),
        Err(actual) => {
            Err(format!("{case}: expected {}, got {}", expected.category(), actual.category()))
        }
        Ok(_) => Err(format!("{case}: expected {}, got success", expected.category())),
    }
}

fn dimension_allocation_cases(limits: &Limits) -> Result<Vec<serde_json::Value>, String> {
    let max_canonical = dimensions(limits, limits.max_width_pixels, limits.max_height_pixels)
        .map_err(|error| format!("max dimensions rejected: {}", error.category()))?;
    if u64::try_from(max_canonical).map_err(|_| "canonical size overflow".to_owned())?
        != limits.max_canonical_rgba_bytes
    {
        return Err("max dimensions do not produce exact canonical ceiling".to_owned());
    }
    expect_rejection(
        &dimensions(limits, limits.max_width_pixels.saturating_add(1), limits.max_height_pixels),
        Rejection::InvalidDimensions,
        "max-plus-one width",
    )?;
    expect_rejection(
        &dimensions(limits, limits.max_width_pixels, limits.max_height_pixels.saturating_add(1)),
        Rejection::InvalidDimensions,
        "max-plus-one height",
    )?;

    let mut max_buffer = Vec::new();
    let mut max_gate = AllocationGate::new(max_canonical);
    max_gate
        .resize(&mut max_buffer, max_canonical)
        .map_err(|error| format!("max canonical allocation: {}", error.category()))?;
    drop(max_buffer);

    let mut denied = Vec::new();
    let mut denied_gate = AllocationGate::new(4095);
    expect_rejection(
        &denied_gate.resize(&mut denied, 4096),
        Rejection::AllocationFailed,
        "injected fallible allocation",
    )?;
    let mut allocator_denied = Vec::<u8>::new();
    if allocator_denied.try_reserve_exact(usize::MAX).is_ok() || !allocator_denied.is_empty() {
        return Err("fallible allocator unexpectedly accepted usize::MAX".to_owned());
    }

    Ok(vec![
        json!({"id": "dimensions_max", "status": "pass", "width": limits.max_width_pixels, "height": limits.max_height_pixels, "canonical_bytes": max_canonical, "allocated_bytes": max_gate.peak}),
        json!({"id": "dimensions_max_plus_one", "status": "pass", "rejection": Rejection::InvalidDimensions.category(), "allocated_bytes": 0}),
        json!({"id": "fallible_allocation", "status": "pass", "rejection": Rejection::AllocationFailed.category(), "requested_bytes": 4096, "injected_ceiling": 4095, "allocator_error_observed": true, "allocated_bytes": denied_gate.peak}),
    ])
}

fn budget_cases(limits: &Limits) -> Result<Vec<serde_json::Value>, String> {
    let cancel = AtomicBool::new(false);
    let mut cancel_guard = WorkGuard::new(limits, &cancel);
    cancel_guard
        .charge(limits.deadline_check_interval_work_units)
        .map_err(|error| format!("cancellation setup: {}", error.category()))?;
    cancel.store(true, Ordering::Release);
    expect_rejection(
        &cancel_guard.charge(limits.deadline_check_interval_work_units),
        Rejection::DecodeCancelled,
        "cooperative cancellation",
    )?;

    let never_cancel = AtomicBool::new(false);
    let mut deadline_guard = WorkGuard::with_deadline(limits, &never_cancel, Duration::ZERO);
    expect_rejection(
        &deadline_guard.charge(limits.deadline_check_interval_work_units),
        Rejection::DecodeDeadlineExceeded,
        "decode deadline",
    )?;

    let mut work_guard = WorkGuard::new(limits, &never_cancel);
    work_guard
        .charge(limits.max_work_units_per_command)
        .map_err(|error| format!("exact work ceiling: {}", error.category()))?;
    expect_rejection(&work_guard.charge(1), Rejection::WorkBudgetExceeded, "work max-plus-one")?;

    Ok(vec![
        json!({"id": "cooperative_cancellation", "status": "pass", "rejection": Rejection::DecodeCancelled.category(), "observed_work_units": cancel_guard.work, "check_interval": limits.deadline_check_interval_work_units}),
        json!({"id": "decode_deadline", "status": "pass", "rejection": Rejection::DecodeDeadlineExceeded.category(), "first_check_work_units": deadline_guard.work, "configured_ms": limits.max_decode_ms}),
        json!({"id": "work_max_plus_one", "status": "pass", "rejection": Rejection::WorkBudgetExceeded.category(), "accepted_work_units": work_guard.work, "limit": limits.max_work_units_per_command}),
    ])
}

fn compression_cases(limits: &Limits) -> Result<Vec<serde_json::Value>, String> {
    let never_cancel = AtomicBool::new(false);
    let zlib_bomb = zlib_zeros(limits.max_inflated_bytes.saturating_add(1))?;
    let canonical_ceiling = usize::try_from(limits.max_canonical_rgba_bytes)
        .map_err(|_| "canonical ceiling does not fit usize".to_owned())?;
    expect_rejection(
        &bounded_inflate(
            &zlib_bomb,
            limits,
            canonical_ceiling,
            &never_cancel,
            Duration::from_millis(limits.max_decode_ms),
        ),
        Rejection::QuotaExceeded,
        "zlib decompression bomb",
    )?;

    let small_zlib = zlib_zeros(5)?;
    let small_png = png_rgba(1, 1, Some(&small_zlib))?;
    let idat = png_idat(
        &small_png,
        usize::try_from(limits.max_base64_decoded_bytes)
            .map_err(|_| "decoded ceiling does not fit usize".to_owned())?,
    )
    .map_err(|error| format!("small PNG IDAT: {}", error.category()))?;
    let inflated = bounded_inflate(
        &idat,
        limits,
        canonical_ceiling,
        &never_cancel,
        Duration::from_millis(limits.max_decode_ms),
    )
    .map_err(|error| format!("small PNG preflight: {}", error.category()))?;
    let decoded_png_bytes = decode_small_png(&small_png, limits)
        .map_err(|error| format!("small PNG decode: {}", error.category()))?;
    if inflated.bytes.len() != 5 || decoded_png_bytes != 4 {
        return Err("small PNG did not decode to one RGBA pixel".to_owned());
    }

    let max_png = png_rgba(limits.max_width_pixels, limits.max_height_pixels, None)?;
    png_header_dimensions(&max_png, limits)
        .map_err(|error| format!("max PNG header: {}", error.category()))?;
    let max_plus_one_png =
        png_rgba(limits.max_width_pixels.saturating_add(1), limits.max_height_pixels, None)?;
    expect_rejection(
        &png_header_dimensions(&max_plus_one_png, limits),
        Rejection::InvalidDimensions,
        "PNG max-plus-one",
    )?;

    let png_bomb = png_rgba(1, 1, Some(&zlib_bomb))?;
    let bomb_idat = png_idat(
        &png_bomb,
        usize::try_from(limits.max_base64_decoded_bytes)
            .map_err(|_| "decoded ceiling does not fit usize".to_owned())?,
    )
    .map_err(|error| format!("bomb PNG IDAT: {}", error.category()))?;
    expect_rejection(
        &bounded_inflate(
            &bomb_idat,
            limits,
            canonical_ceiling,
            &never_cancel,
            Duration::from_millis(limits.max_decode_ms),
        ),
        Rejection::QuotaExceeded,
        "PNG decompression bomb",
    )?;

    Ok(vec![
        json!({"id": "zlib_bomb", "status": "pass", "rejection": Rejection::QuotaExceeded.category(), "inflated_limit": limits.max_inflated_bytes, "bomb_uncompressed_bytes": limits.max_inflated_bytes + 1, "compressed_bytes": zlib_bomb.len()}),
        json!({"id": "png_valid", "status": "pass", "decoded_rgba_bytes": 4, "preflight_work_units": inflated.work, "deadline_checks": inflated.checks}),
        json!({"id": "png_bomb", "status": "pass", "rejection": Rejection::QuotaExceeded.category(), "inflated_limit": limits.max_inflated_bytes}),
        json!({"id": "png_max_plus_one", "status": "pass", "rejection": Rejection::InvalidDimensions.category(), "allocated_bytes": 0}),
    ])
}

fn sixel_growth_case(limits: &Limits) -> Result<serde_json::Value, String> {
    let (growth_rejection, growth_peak, rejected_width) = gradual_sixel_growth(limits)?;
    if growth_rejection != Rejection::InvalidDimensions
        || rejected_width != limits.max_width_pixels.saturating_add(1)
        || growth_peak
            != usize::try_from(u64::from(limits.max_width_pixels) * 4)
                .map_err(|_| "growth peak overflow".to_owned())?
    {
        return Err("gradual Sixel growth crossed a dimension or allocation bound".to_owned());
    }
    Ok(
        json!({"id": "sixel_gradual_growth", "status": "pass", "rejection": growth_rejection.category(), "rejected_width": rejected_width, "peak_allocated_bytes": growth_peak}),
    )
}

fn publish_evidence(evidence_path: &Path, evidence: &serde_json::Value) -> Result<(), String> {
    let parent = evidence_path
        .parent()
        .ok_or_else(|| format!("evidence path has no parent: {}", evidence_path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = evidence_path.with_extension("json.tmp");
    let mut encoded = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("serialize evidence: {error}"))?;
    encoded.push(b'\n');
    fs::write(&temporary, encoded)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, evidence_path).map_err(|error| {
        format!("publish {} as {}: {error}", temporary.display(), evidence_path.display())
    })?;
    Ok(())
}

/// Run every spike scenario and atomically publish JSON evidence.
pub fn run(contract_path: &Path, evidence_path: &Path) -> Result<(), String> {
    let contract_bytes = fs::read(contract_path)
        .map_err(|error| format!("read {}: {error}", contract_path.display()))?;
    let contract: Contract = serde_json::from_slice(&contract_bytes)
        .map_err(|error| format!("parse {}: {error}", contract_path.display()))?;
    let limits = contract.limits;
    if limits.deadline_check_interval_work_units != 4096 {
        return Err("contract deadline interval is not 4096 work units".to_owned());
    }
    let mut cases = dimension_allocation_cases(&limits)?;
    cases.extend(budget_cases(&limits)?);
    cases.extend(compression_cases(&limits)?);
    cases.push(sixel_growth_case(&limits)?);
    let evidence = json!({
        "schema_version": 1,
        "contract_version": contract.contract_version,
        "decision": "conditional_go",
        "all_passed": true,
        "limits": limits,
        "library_boundaries": {
            "zlib": "go: flate2::Decompress low-level bounded loop",
            "png": "fork-required: image-png decoder core with Scribe work/cancel hook",
            "sixel": "fork-required: icy_sixel 0.5.0 decoder-only with DecodeLimits",
            "generic_image_decode": "no-go"
        },
        "cases": cases
    });
    publish_evidence(evidence_path, &evidence)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "PASS: bounded terminal-image decode spike completed")
        .map_err(|error| format!("write completion status: {error}"))?;
    Ok(())
}
