//! Adversarial Docker evidence for bounded Kitty payload normalization.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use scribe_common::kitty_decode::{
    KittyCompression, KittyDataParams, KittyDecodeError, KittyFormat, KittyTransfer,
    KittyTransport, NormalizedKittyImage,
};
use scribe_common::terminal_images::{ImageLimits, TerminalImageRejectionReason};
use scribe_image_decode::{AllocationDenied, DecodeBudget, DecodeHooks, DecodeLimits, NoopHooks};
use scribe_png_decoder::{PngLimits, decode_png};
use serde::Deserialize;
use serde_json::json;

use crate::decode_storage::decode_storage;

const PNG_FIXTURE_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg==";

#[derive(Debug, Deserialize)]
struct Contract {
    contract_version: String,
    limits: ImageLimits,
}

fn params(
    format: KittyFormat,
    compression: KittyCompression,
    width: Option<u32>,
    height: Option<u32>,
) -> KittyDataParams {
    KittyDataParams { format, transport: KittyTransport::Direct, compression, width, height }
}

fn decode_limits(contract: ImageLimits) -> DecodeLimits {
    DecodeLimits {
        max_width_pixels: contract.max_width_pixels as usize,
        max_height_pixels: contract.max_height_pixels as usize,
        max_pixels: usize::try_from(contract.max_pixels).unwrap_or(usize::MAX),
        max_rgba_bytes: usize::try_from(contract.max_canonical_rgba_bytes).unwrap_or(usize::MAX),
        max_work_units: contract.max_work_units_per_command,
        deadline: Instant::now() + Duration::from_millis(contract.max_decode_ms),
        check_interval_work_units: contract.deadline_check_interval_work_units,
    }
}

fn normalize(
    params: KittyDataParams,
    chunks: &[(&[u8], bool)],
    limits: ImageLimits,
    budget_limits: DecodeLimits,
    hooks: &impl DecodeHooks,
) -> Result<NormalizedKittyImage, KittyDecodeError> {
    let storage = decode_storage();
    let mut budget =
        DecodeBudget::new(budget_limits, hooks, &storage).map_err(KittyDecodeError::from)?;
    let mut transfer = KittyTransfer::new(params, limits)?;
    for &(chunk, more) in chunks {
        transfer.push_chunk(chunk, more, &mut budget)?;
    }
    transfer.finish(&mut budget)
}

fn expect_reason(
    result: &Result<NormalizedKittyImage, KittyDecodeError>,
    expected: TerminalImageRejectionReason,
    context: &str,
) -> Result<(), String> {
    match result {
        Err(error) if error.reason == expected => Ok(()),
        Err(error) => Err(format!("{context}: expected {expected:?}, got {:?}", error.reason)),
        Ok(_) => Err(format!("{context}: expected {expected:?}, operation succeeded")),
    }
}

fn success_cases(limits: ImageLimits) -> Result<Vec<serde_json::Value>, String> {
    let rgb = normalize(
        params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
        &[(b"/wAA", false)],
        limits,
        decode_limits(limits),
        &NoopHooks,
    )
    .map_err(|error| format!("RGB: {error}"))?;
    if rgb.rgba != [255, 0, 0, 255] || rgb.width != 1 || rgb.height != 1 {
        return Err("RGB normalization drifted".to_owned());
    }

    let rgba = normalize(
        params(KittyFormat::Rgba, KittyCompression::None, Some(1), Some(1)),
        &[(b"/wAAgA==", false)],
        limits,
        decode_limits(limits),
        &NoopHooks,
    )
    .map_err(|error| format!("RGBA: {error}"))?;
    if rgba.rgba != [255, 0, 0, 128] || !rgba.has_alpha {
        return Err("RGBA normalization drifted".to_owned());
    }

    let chunked = normalize(
        params(KittyFormat::Rgb, KittyCompression::None, Some(2), Some(1)),
        &[(b"/wAA", true), (b"AP8A", false)],
        limits,
        decode_limits(limits),
        &NoopHooks,
    )
    .map_err(|error| format!("chunked RGB: {error}"))?;
    if chunked.rgba != [255, 0, 0, 255, 0, 255, 0, 255] {
        return Err("chunk accumulation drifted".to_owned());
    }

    let zlib = normalize(
        params(KittyFormat::Rgba, KittyCompression::Rfc1950Zlib, Some(1), Some(1)),
        &[(b"eJz7z8DQ", true), (b"AAAEgAGA", false)],
        limits,
        decode_limits(limits),
        &NoopHooks,
    )
    .map_err(|error| format!("zlib RGBA: {error}"))?;
    if zlib.rgba != [255, 0, 0, 128] || zlib.inflated_bytes != 4 {
        return Err("zlib normalization drifted".to_owned());
    }

    let png_bytes = STANDARD.decode(PNG_FIXTURE_BASE64).map_err(|error| error.to_string())?;
    let png_storage = decode_storage();
    let mut png_budget = DecodeBudget::new(decode_limits(limits), &NoopHooks, &png_storage)
        .map_err(|error| format!("PNG budget: {error}"))?;
    decode_png(&png_bytes, png_limits(limits), &mut png_budget)
        .map_err(|error| format!("direct PNG fork: {error:?} ({error})"))?;
    let png = normalize(
        params(KittyFormat::Png, KittyCompression::None, None, None),
        &[(PNG_FIXTURE_BASE64.as_bytes(), false)],
        limits,
        decode_limits(limits),
        &NoopHooks,
    )
    .map_err(|error| format!("PNG: {error}"))?;
    if png.width != 1 || png.height != 1 || png.rgba.len() != 4 {
        return Err("PNG normalization drifted".to_owned());
    }

    Ok(vec![
        json!({"id":"rgb","status":"pass","rgba_bytes":4}),
        json!({"id":"rgba","status":"pass","rgba_bytes":4}),
        json!({"id":"chunked","status":"pass","chunks":2}),
        json!({"id":"zlib","status":"pass","inflated_bytes":4}),
        json!({"id":"png","status":"pass","width":1,"height":1,"rgba_bytes":4}),
    ])
}

fn malformed_cases(limits: ImageLimits) -> Result<Vec<serde_json::Value>, String> {
    expect_reason(
        &normalize(
            params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
            &[(b"****", false)],
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::MalformedPayload,
        "malformed base64",
    )?;
    expect_reason(
        &normalize(
            params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
            &[(b"/w==", true), (b"AAAA", false)],
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::ChunkMismatch,
        "non-final padding",
    )?;
    expect_reason(
        &normalize(
            params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
            &[(b"/wAAAA==", false)],
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::MalformedPayload,
        "raw length mismatch",
    )?;
    expect_reason(
        &normalize(
            params(KittyFormat::Png, KittyCompression::None, None, None),
            &[(b"R0lGODlh", false)],
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::MalformedPayload,
        "non-PNG format",
    )?;

    let mut truncated = STANDARD.decode(PNG_FIXTURE_BASE64).map_err(|error| error.to_string())?;
    let _ = truncated.pop();
    let truncated_base64 = STANDARD.encode(truncated);
    expect_reason(
        &normalize(
            params(KittyFormat::Png, KittyCompression::None, None, None),
            &[(truncated_base64.as_bytes(), false)],
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::MalformedPayload,
        "truncated PNG",
    )?;

    Ok(vec![
        json!({"id":"malformed_base64","status":"pass","rejection":"malformed_payload"}),
        json!({"id":"chunk_mismatch","status":"pass","rejection":"chunk_mismatch"}),
        json!({"id":"raw_length_mismatch","status":"pass","rejection":"malformed_payload"}),
        json!({"id":"non_png_format","status":"pass","rejection":"malformed_payload"}),
        json!({"id":"truncated_png","status":"pass","rejection":"malformed_payload"}),
    ])
}

struct CountAllocations(AtomicUsize);

impl DecodeHooks for CountAllocations {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn before_allocation(&self, _requested_bytes: usize) -> Result<(), AllocationDenied> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

struct DenyAllocations;

impl DecodeHooks for DenyAllocations {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn before_allocation(&self, _requested_bytes: usize) -> Result<(), AllocationDenied> {
        Err(AllocationDenied)
    }
}

struct CancelImmediately;

impl DecodeHooks for CancelImmediately {
    fn is_cancelled(&self) -> bool {
        true
    }
}

fn rejection_cases(limits: ImageLimits) -> Result<Vec<serde_json::Value>, String> {
    for transport in [
        KittyTransport::File,
        KittyTransport::TemporaryFile,
        KittyTransport::SharedMemory,
        KittyTransport::OtherIndirect,
    ] {
        let hooks = CountAllocations(AtomicUsize::new(0));
        let mut direct = params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1));
        direct.transport = transport;
        expect_reason(
            &normalize(direct, &[(b"/wAA", false)], limits, decode_limits(limits), &hooks),
            TerminalImageRejectionReason::UnsupportedTransport,
            "indirect transport",
        )?;
        if hooks.0.load(Ordering::Acquire) != 0 {
            return Err("indirect transport allocated before rejection".to_owned());
        }
    }

    expect_reason(
        &normalize(
            params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
            &[(b"/wAA", false)],
            limits,
            decode_limits(limits),
            &DenyAllocations,
        ),
        TerminalImageRejectionReason::QuotaExceeded,
        "allocation denial",
    )?;

    let mut expired = decode_limits(limits);
    expired.deadline = Instant::now();
    expect_reason(
        &normalize(
            params(KittyFormat::Rgb, KittyCompression::None, Some(1), Some(1)),
            &[(b"/wAA", false)],
            limits,
            expired,
            &NoopHooks,
        ),
        TerminalImageRejectionReason::DecodeDeadlineExceeded,
        "deadline",
    )?;

    let cancelled_storage = decode_storage();
    let cancelled =
        DecodeBudget::new(decode_limits(limits), &CancelImmediately, &cancelled_storage);
    if !matches!(cancelled, Err(scribe_image_decode::BudgetError::DecodeCancelled { .. })) {
        return Err("immediate cancellation did not reject".to_owned());
    }

    Ok(vec![
        json!({"id":"indirect_sources","status":"pass","rejection":"unsupported_transport","allocations":0}),
        json!({"id":"allocation_failure","status":"pass","rejection":"quota_exceeded"}),
        json!({"id":"deadline","status":"pass","rejection":"decode_deadline_exceeded"}),
        json!({"id":"cancellation","status":"pass","rejection":"decode_cancelled"}),
    ])
}

fn bomb_case(limits: ImageLimits) -> Result<serde_json::Value, String> {
    let mut zlib_writer = ZlibEncoder::new(Vec::new(), Compression::fast());
    let block = [0u8; 4_096];
    for _ in 0..(limits.max_inflated_bytes / block.len() as u64) {
        zlib_writer.write_all(&block).map_err(|error| format!("zlib bomb generation: {error}"))?;
    }
    zlib_writer.write_all(&[0]).map_err(|error| format!("zlib bomb tail: {error}"))?;
    let compressed = zlib_writer.finish().map_err(|error| format!("zlib bomb finish: {error}"))?;
    let bomb_base64 = STANDARD.encode(compressed);
    let chunks: Vec<(&[u8], bool)> = bomb_base64
        .as_bytes()
        .chunks(4_096)
        .enumerate()
        .map(|(index, chunk)| {
            let more = (index + 1) * 4_096 < bomb_base64.len();
            (chunk, more)
        })
        .collect();
    expect_reason(
        &normalize(
            params(KittyFormat::Rgba, KittyCompression::Rfc1950Zlib, Some(1), Some(1)),
            &chunks,
            limits,
            decode_limits(limits),
            &NoopHooks,
        ),
        TerminalImageRejectionReason::WorkBudgetExceeded,
        "zlib bomb default work limit",
    )?;
    let mut quota_isolation = decode_limits(limits);
    quota_isolation.max_work_units = u64::MAX;
    expect_reason(
        &normalize(
            params(KittyFormat::Rgba, KittyCompression::Rfc1950Zlib, Some(1), Some(1)),
            &chunks,
            limits,
            quota_isolation,
            &NoopHooks,
        ),
        TerminalImageRejectionReason::QuotaExceeded,
        "zlib bomb isolated inflated quota",
    )?;
    Ok(json!({
        "id":"zlib_bomb",
        "status":"pass",
        "default_rejection":"work_budget_exceeded",
        "isolated_rejection":"quota_exceeded",
        "limit":"inflated_bytes",
        "attempted":limits.max_inflated_bytes + 1
    }))
}

fn png_limits(limits: ImageLimits) -> PngLimits {
    PngLimits {
        max_width_pixels: limits.max_width_pixels as usize,
        max_height_pixels: limits.max_height_pixels as usize,
        max_pixels: usize::try_from(limits.max_pixels).unwrap_or(usize::MAX),
        max_inflated_bytes: usize::try_from(limits.max_inflated_bytes).unwrap_or(usize::MAX),
        max_rgba_bytes: usize::try_from(limits.max_canonical_rgba_bytes).unwrap_or(usize::MAX),
    }
}

fn publish(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let parent =
        path.parent().ok_or_else(|| format!("evidence path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("serialize evidence: {error}"))?;
    bytes.push(b'\n');
    fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("publish {}: {error}", path.display()))
}

/// Run the full Kitty corpus and atomically publish schema-versioned evidence.
pub fn run(contract_path: &Path, evidence: &Path) -> Result<(), String> {
    let bytes = fs::read(contract_path)
        .map_err(|error| format!("read {}: {error}", contract_path.display()))?;
    let contract: Contract = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", contract_path.display()))?;
    if contract.limits != ImageLimits::V1 {
        return Err("contract limits drifted from ImageLimits::V1".to_owned());
    }
    let mut cases = success_cases(contract.limits)?;
    cases.extend(malformed_cases(contract.limits)?);
    cases.extend(rejection_cases(contract.limits)?);
    cases.push(bomb_case(contract.limits)?);
    let evidence_value = json!({
        "schema_version": 1,
        "contract_version": contract.contract_version,
        "all_passed": true,
        "normalizer": "scribe-common bounded Kitty v1",
        "dependencies": {
            "base64": {"version":"0.22.1","revision":"e14400697453bcc85997119b874bc03d9601d0af","sha256":"72b3254f16251a8381aa12e40e3c4d2f0199f8c6508fbecb9d91f575e0fbb8c6"},
            "flate2": {"version":"1.1.9","revision":"19ddb18bf11199858fbc6504d079448fafd1606e","sha256":"843fba2746e448b37e26a819579957415c8cef339bf08564fe8b7ddbd959573c"},
            "png": {"version":"0.18.1-scribe.1","upstream_revision":"2a3f980245e3ae38b82ade96533e7b450e8477bb","sha256":"60769b8b31b2a9f263dae2776c37b1b28ae246943cf719eb6946a1db05128a61","license":"MIT OR Apache-2.0"}
        },
        "excluded": ["indirect_sources","generic_image","non_png_formats","encoder","apng","text_chunks","profiles"],
        "limits": contract.limits,
        "cases": cases
    });
    publish(evidence, &evidence_value)?;
    writeln!(std::io::stdout().lock(), "PASS: bounded Kitty normalization corpus completed")
        .map_err(|error| format!("write status: {error}"))?;
    Ok(())
}
