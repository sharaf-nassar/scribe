//! Adversarial Docker evidence for the vendored bounded Sixel decoder.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use icy_sixel_decoder::{
    AllocationDenied, BackgroundMode, DcsSettings, DecodeError, DecodeHooks, DecodeLimits,
    DecodedSixel, NoopHooks, decode_sixel as decode_sixel_accounted,
    decode_sixel_payload as decode_sixel_payload_accounted,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::decode_storage::decode_storage;

fn decode_sixel(
    data: &[u8],
    limits: DecodeLimits,
    hooks: &impl DecodeHooks,
) -> Result<DecodedSixel, DecodeError> {
    let storage = decode_storage();
    decode_sixel_accounted(data, limits, hooks, &storage)
}

fn decode_sixel_payload(
    payload: &[u8],
    settings: DcsSettings,
    limits: DecodeLimits,
    hooks: &impl DecodeHooks,
) -> Result<DecodedSixel, DecodeError> {
    let storage = decode_storage();
    decode_sixel_payload_accounted(payload, settings, limits, hooks, &storage)
}

#[derive(Debug, Deserialize)]
struct Contract {
    contract_version: String,
    limits: ContractLimits,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ContractLimits {
    max_width_pixels: usize,
    max_height_pixels: usize,
    max_pixels: usize,
    max_canonical_rgba_bytes: usize,
    max_work_units_per_command: u64,
    max_decode_ms: u64,
    deadline_check_interval_work_units: u64,
}

impl ContractLimits {
    fn decoder(self) -> DecodeLimits {
        DecodeLimits {
            max_width_pixels: self.max_width_pixels,
            max_height_pixels: self.max_height_pixels,
            max_pixels: self.max_pixels,
            max_rgba_bytes: self.max_canonical_rgba_bytes,
            max_work_units: self.max_work_units_per_command,
            deadline: Instant::now() + Duration::from_millis(self.max_decode_ms),
            check_interval_work_units: self.deadline_check_interval_work_units,
        }
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

struct CancelAfterCheck {
    observations: AtomicUsize,
}

impl DecodeHooks for CancelAfterCheck {
    fn is_cancelled(&self) -> bool {
        self.observations.fetch_add(1, Ordering::AcqRel) >= 1
    }
}

fn decode_hex(path: &Path) -> Result<Vec<u8>, String> {
    let encoded =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let encoded = encoded.trim();
    if encoded.len() % 2 != 0 {
        return Err(format!("fixture has odd hex length: {}", path.display()));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(encoded.len() / 2)
        .map_err(|_| format!("fixture allocation failed: {}", path.display()))?;
    for offset in (0..encoded.len()).step_by(2) {
        let pair = encoded
            .get(offset..offset + 2)
            .ok_or_else(|| format!("fixture slice failed: {}", path.display()))?;
        let byte = u8::from_str_radix(pair, 16)
            .map_err(|error| format!("fixture hex {}: {error}", path.display()))?;
        output.push(byte);
    }
    Ok(output)
}

fn expect_category<T>(
    result: Result<T, DecodeError>,
    category: &str,
    context: &str,
) -> Result<DecodeError, String> {
    match result {
        Ok(_) => Err(format!("{context}: expected {category}, decode succeeded")),
        Err(error) if error.category() == category => Ok(error),
        Err(error) => {
            Err(format!("{context}: expected {category}, got {} ({error})", error.category()))
        }
    }
}

fn fixture_cases(
    fixtures: &Path,
    limits: ContractLimits,
) -> Result<Vec<serde_json::Value>, String> {
    let opaque = decode_hex(&fixtures.join("sixel-7bit.hex"))?;
    let opaque_image = decode_sixel(&opaque, limits.decoder(), &NoopHooks)
        .map_err(|error| format!("7-bit fixture: {error}"))?;
    if opaque_image.width != 1
        || opaque_image.height != 6
        || opaque_image.background_mode != BackgroundMode::Opaque
        || opaque_image.rgba.get(0..4) != Some(&[255, 0, 0, 255])
    {
        return Err("7-bit fixture did not produce the frozen red column".to_owned());
    }

    let transparent = decode_hex(&fixtures.join("sixel-c1-transparent.hex"))?;
    let transparent_image = decode_sixel(&transparent, limits.decoder(), &NoopHooks)
        .map_err(|error| format!("C1 fixture: {error}"))?;
    if transparent_image.width != 1
        || transparent_image.height != 6
        || transparent_image.background_mode != BackgroundMode::Transparent
    {
        return Err("C1 fixture did not preserve transparent mode".to_owned());
    }

    Ok(vec![
        json!({"id": "owned_7bit_fixture", "status": "pass", "width": 1, "height": 6, "rgba_bytes": 24}),
        json!({"id": "owned_c1_transparent_fixture", "status": "pass", "width": 1, "height": 6}),
    ])
}

fn semantic_cases(limits: ContractLimits) -> Result<Vec<serde_json::Value>, String> {
    let opaque = decode_sixel(b"\x1bP0;2q?\x1b\\", limits.decoder(), &NoopHooks)
        .map_err(|error| format!("opaque background: {error}"))?;
    let transparent = decode_sixel(b"\x1bP0;1q?\x1b\\", limits.decoder(), &NoopHooks)
        .map_err(|error| format!("transparent background: {error}"))?;
    if !opaque.rgba.chunks_exact(4).all(|pixel| pixel.get(3) == Some(&255))
        || !transparent.rgba.chunks_exact(4).all(|pixel| pixel == [0, 0, 0, 0])
    {
        return Err("P2 background semantics drifted".to_owned());
    }

    let repeat = decode_sixel(b"\x1bPq#7;2;100;0;0!4~\x1b\\", limits.decoder(), &NoopHooks)
        .map_err(|error| format!("repeat/palette: {error}"))?;
    if repeat.width != 4
        || repeat.height != 6
        || !repeat.rgba.chunks_exact(4).all(|pixel| pixel == [255, 0, 0, 255])
    {
        return Err("palette RGB or repeat semantics drifted".to_owned());
    }

    let raster =
        decode_sixel(b"\x1bPq\"1;1;3;2#1;2;0;100;0#1~\x1b\\", limits.decoder(), &NoopHooks)
            .map_err(|error| format!("raster attributes: {error}"))?;
    if raster.width != 3 || raster.height != 6 {
        return Err("raster attributes did not establish bounded extent".to_owned());
    }

    Ok(vec![
        json!({"id": "background_modes", "status": "pass", "opaque_alpha": 255, "transparent_alpha": 0}),
        json!({"id": "palette_repeat", "status": "pass", "width": 4, "height": 6}),
        json!({"id": "raster_attributes", "status": "pass", "declared_width": 3, "declared_height": 2, "decoded_height": 6}),
    ])
}

fn dimension_cases(limits: ContractLimits) -> Result<Vec<serde_json::Value>, String> {
    let max_width = format!("\x1bPq\"1;1;{};1?\x1b\\", limits.max_width_pixels);
    let max = decode_sixel(max_width.as_bytes(), limits.decoder(), &NoopHooks)
        .map_err(|error| format!("maximum width: {error}"))?;
    if max.width != limits.max_width_pixels {
        return Err("maximum width was not preserved".to_owned());
    }

    let plus_one = format!("\x1bPq\"1;1;{};1?\x1b\\", limits.max_width_pixels.saturating_add(1));
    expect_category(
        decode_sixel(plus_one.as_bytes(), limits.decoder(), &NoopHooks),
        "invalid_dimensions",
        "width max-plus-one",
    )?;

    let repeat_max = format!("\x1bPq!{}~\x1b\\", limits.max_width_pixels);
    let repeated = decode_sixel(repeat_max.as_bytes(), limits.decoder(), &NoopHooks)
        .map_err(|error| format!("repeat maximum: {error}"))?;
    if repeated.width != limits.max_width_pixels {
        return Err("maximum repeat width was not preserved".to_owned());
    }
    let repeat_plus_one = format!("\x1bPq!{}~\x1b\\", limits.max_width_pixels.saturating_add(1));
    expect_category(
        decode_sixel(repeat_plus_one.as_bytes(), limits.decoder(), &NoopHooks),
        "invalid_dimensions",
        "repeat max-plus-one",
    )?;

    Ok(vec![
        json!({"id": "dimensions_max", "status": "pass", "width": limits.max_width_pixels}),
        json!({"id": "dimensions_max_plus_one", "status": "pass", "rejection": "invalid_dimensions", "width": limits.max_width_pixels + 1}),
        json!({"id": "repeat_growth_max", "status": "pass", "width": limits.max_width_pixels}),
        json!({"id": "repeat_growth_max_plus_one", "status": "pass", "rejection": "invalid_dimensions", "width": limits.max_width_pixels + 1}),
    ])
}

fn failure_cases(limits: ContractLimits) -> Result<Vec<serde_json::Value>, String> {
    expect_category(
        decode_sixel(b"\x1bPq~\x1b\\", limits.decoder(), &DenyAllocations),
        "allocation_failed",
        "allocation injection",
    )?;
    expect_category(
        decode_sixel(b"\x1bPq~\x1b\\", limits.decoder(), &CancelImmediately),
        "decode_cancelled",
        "immediate cancellation",
    )?;

    let mut deadline = limits.decoder();
    deadline.deadline = Instant::now();
    expect_category(
        decode_sixel(b"\x1bPq~\x1b\\", deadline, &NoopHooks),
        "decode_deadline_exceeded",
        "expired deadline",
    )?;

    let cancellation = CancelAfterCheck { observations: AtomicUsize::new(0) };
    expect_category(
        decode_sixel(b"\x1bPq!4096~\x1b\\", limits.decoder(), &cancellation),
        "decode_cancelled",
        "cooperative cancellation",
    )?;

    expect_category(
        decode_sixel(b"\x1bPq~", limits.decoder(), &NoopHooks),
        "malformed_payload",
        "truncated sequence",
    )?;
    expect_category(
        decode_sixel(b"\x1bPq!999999999999999999999999999999~\x1b\\", limits.decoder(), &NoopHooks),
        "malformed_payload",
        "numeric overflow",
    )?;
    expect_category(
        decode_sixel(b"\x1bPq#256~\x1b\\", limits.decoder(), &NoopHooks),
        "quota_exceeded",
        "palette max-plus-one",
    )?;
    let palette_max = decode_sixel(b"\x1bPq#255~\x1b\\", limits.decoder(), &NoopHooks)
        .map_err(|error| format!("palette maximum: {error}"))?;
    if palette_max.width != 1 {
        return Err("palette maximum did not decode".to_owned());
    }

    Ok(vec![
        json!({"id": "allocation_failure", "status": "pass", "rejection": "allocation_failed"}),
        json!({"id": "cancellation_immediate", "status": "pass", "rejection": "decode_cancelled"}),
        json!({"id": "deadline", "status": "pass", "rejection": "decode_deadline_exceeded"}),
        json!({"id": "cancellation_cooperative", "status": "pass", "rejection": "decode_cancelled", "check_interval": limits.deadline_check_interval_work_units}),
        json!({"id": "malformed_truncated", "status": "pass", "rejection": "malformed_payload"}),
        json!({"id": "numeric_overflow", "status": "pass", "rejection": "malformed_payload"}),
        json!({"id": "palette_max", "status": "pass", "register": 255}),
        json!({"id": "palette_max_plus_one", "status": "pass", "rejection": "quota_exceeded", "register": 256}),
    ])
}

fn work_cases(limits: ContractLimits) -> Result<Vec<serde_json::Value>, String> {
    let baseline = decode_sixel_payload(b"~", DcsSettings::default(), limits.decoder(), &NoopHooks)
        .map_err(|error| format!("work baseline: {error}"))?;
    let exact_work = baseline.stats.work_units;

    let mut exact = limits.decoder();
    exact.max_work_units = exact_work;
    decode_sixel_payload(b"~", DcsSettings::default(), exact, &NoopHooks)
        .map_err(|error| format!("work exact maximum: {error}"))?;

    let mut exceeded = limits.decoder();
    exceeded.max_work_units = exact_work.saturating_sub(1);
    expect_category(
        decode_sixel_payload(b"~", DcsSettings::default(), exceeded, &NoopHooks),
        "work_budget_exceeded",
        "work max-plus-one",
    )?;

    Ok(vec![
        json!({"id": "work_max", "status": "pass", "work_units": exact_work}),
        json!({"id": "work_max_plus_one", "status": "pass", "rejection": "work_budget_exceeded", "required_work_units": exact_work, "configured_work_units": exact_work - 1}),
    ])
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
    fs::rename(&temporary, path)
        .map_err(|error| format!("publish {} as {}: {error}", temporary.display(), path.display()))
}

/// Run the complete bounded Sixel corpus and atomically publish evidence.
pub fn run(contract_path: &Path, fixtures: &Path, evidence: &Path) -> Result<(), String> {
    let contract_bytes = fs::read(contract_path)
        .map_err(|error| format!("read {}: {error}", contract_path.display()))?;
    let contract: Contract = serde_json::from_slice(&contract_bytes)
        .map_err(|error| format!("parse {}: {error}", contract_path.display()))?;
    if contract.limits
        != (ContractLimits {
            max_width_pixels: 4_096,
            max_height_pixels: 4_096,
            max_pixels: 16_777_216,
            max_canonical_rgba_bytes: 67_108_864,
            max_work_units_per_command: 134_217_728,
            max_decode_ms: 2_000,
            deadline_check_interval_work_units: 4_096,
        })
    {
        return Err("contract decode limits drifted from terminal-images-v1".to_owned());
    }

    let mut cases = fixture_cases(fixtures, contract.limits)?;
    cases.extend(semantic_cases(contract.limits)?);
    cases.extend(dimension_cases(contract.limits)?);
    cases.extend(failure_cases(contract.limits)?);
    cases.extend(work_cases(contract.limits)?);
    let evidence_value = json!({
        "schema_version": 1,
        "contract_version": contract.contract_version,
        "decoder": "icy-sixel-decoder 0.5.0-scribe.1",
        "upstream": {
            "crate": "icy_sixel 0.5.0",
            "url": "https://crates.io/crates/icy_sixel/0.5.0",
            "revision": "998cbb2c6d8ed5272f9cc4702a4660778972bf3f",
            "sha256": "85518b9086bf01117761b90e7691c0ef3236fa8adfb1fb44dd248fe5f87215d5",
            "license": "MIT OR Apache-2.0"
        },
        "excluded": ["encoder", "quantette", "simd_spans", "termwiz", "c_sixel_libraries"],
        "limits": contract.limits,
        "all_passed": true,
        "cases": cases
    });
    publish(evidence, &evidence_value)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "PASS: bounded vendored Sixel decoder corpus completed")
        .map_err(|error| format!("write completion status: {error}"))?;
    Ok(())
}
