//! Harness-only terminal-images-v1 framing and parser verification.

use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

use scribe_image_decode::{DecodeStorage, StorageProcess, StorageValidation};

use scribe_pty::graphics_framing::{
    GraphicsEvent, GraphicsFailureCategory, GraphicsFramer, GraphicsLimit, GraphicsProtocol,
    GraphicsStorageBudget, KittyAction, KittyChunkState, KittyCommand, KittyCompression,
    KittyDelete, KittyFormat, KittyPlacementMode, MAX_CONTROL_STRING_BYTES, RawByteRange,
    SixelMode, SixelParameters,
};
use serde::Serialize;
use serde_json::json;

const OWNED_FIXTURES: [&str; 10] = [
    "kitty-query-order.hex",
    "kitty-rgb-classic.hex",
    "kitty-rgba-zlib-chunked.hex",
    "kitty-png-classic.hex",
    "kitty-unicode-placeholder.hex",
    "kitty-delete-lifecycle.hex",
    "sixel-7bit.hex",
    "sixel-c1-transparent.hex",
    "sixel-mode-chronology.hex",
    "malformed-recovery.hex",
];

fn validation_framer(max: Option<usize>) -> GraphicsFramer {
    let budget: Arc<GraphicsStorageBudget> = DecodeStorage::new(
        StorageProcess::new(u64::MAX),
        u64::MAX,
        0,
        StorageValidation::default(),
    );
    GraphicsFramer::with_storage_budget(max.unwrap_or(MAX_CONTROL_STRING_BYTES), budget)
}

#[derive(Serialize)]
struct Evidence {
    schema_version: u32,
    contract_version: &'static str,
    status: &'static str,
    all_passed: bool,
    payload_bytes_recorded: bool,
    aggregates: EvidenceAggregates,
    fixtures: Vec<FixtureEvidence>,
    cases: Vec<CaseEvidence>,
}

#[derive(Serialize)]
struct EvidenceAggregates {
    owned_fixture_count: usize,
    fixture_split_points_verified: usize,
    fixture_one_byte_read_cases: usize,
    fixture_semantic_cases: usize,
    raw_range_tiling_cases: usize,
}

#[derive(Serialize)]
struct FixtureEvidence {
    id: String,
    byte_length: usize,
    split_points_verified: usize,
    one_byte_reads_verified: bool,
    raw_ranges_tile: bool,
    terminal_bytes_match_source_ranges: &'static str,
}

#[derive(Serialize)]
struct CaseEvidence {
    id: &'static str,
    status: &'static str,
    facts: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    forwarded: Vec<u8>,
    annotations: Vec<GraphicsEvent>,
}

pub fn run(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let mut fixture_evidence = Vec::with_capacity(OWNED_FIXTURES.len());
    for name in OWNED_FIXTURES {
        let bytes = read_hex(&fixtures.join(name))?;
        assert_owned_fixture_every_split(name, &bytes)?;
        fixture_evidence.push(FixtureEvidence {
            id: name.trim_end_matches(".hex").to_owned(),
            byte_length: bytes.len(),
            split_points_verified: bytes.len().saturating_add(1),
            one_byte_reads_verified: true,
            raw_ranges_tile: true,
            terminal_bytes_match_source_ranges: "pass",
        });
    }

    verify_interruption_and_recovery()?;
    verify_malformed_and_unsupported()?;
    verify_overlap_safe_termination()?;
    verify_candidate_resynchronization()?;
    verify_sixel_parameter_bounds()?;
    verify_unterminated_recovery()?;
    verify_exact_and_over_budget()?;
    verify_sixel_header_quota_boundary()?;
    verify_cancellation_preserves_first_failure()?;
    verify_non_image_passthrough()?;
    publish_evidence(evidence_path, fixture_evidence)?;

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "PASS: bounded terminal-image framing and parsers")
        .map_err(|error| error.to_string())
}

fn publish_evidence(evidence_path: &Path, fixtures: Vec<FixtureEvidence>) -> Result<(), String> {
    let parent = evidence_path
        .parent()
        .ok_or_else(|| format!("evidence path has no parent: {}", evidence_path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let fixture_split_points_verified: usize =
        fixtures.iter().map(|fixture| fixture.split_points_verified).sum();
    let fixture_count = fixtures.len();
    let raw_range_tiling_cases = fixture_split_points_verified.saturating_add(fixture_count);
    let evidence = Evidence {
        schema_version: 1,
        contract_version: "terminal-images-v1",
        status: "pass",
        all_passed: true,
        payload_bytes_recorded: false,
        aggregates: EvidenceAggregates {
            owned_fixture_count: fixture_count,
            fixture_split_points_verified,
            fixture_one_byte_read_cases: fixture_count,
            fixture_semantic_cases: raw_range_tiling_cases,
            raw_range_tiling_cases,
        },
        fixtures,
        cases: evidence_cases(),
    };
    let mut encoded = serde_json::to_vec_pretty(&evidence)
        .map_err(|error| format!("serialize framing evidence: {error}"))?;
    encoded.push(b'\n');
    let temporary = evidence_path.with_extension("json.tmp");
    fs::write(&temporary, encoded)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, evidence_path).map_err(|error| {
        format!("publish {} as {}: {error}", temporary.display(), evidence_path.display())
    })
}

fn evidence_cases() -> Vec<CaseEvidence> {
    let mut cases = core_evidence_cases();
    cases.extend(hardening_evidence_cases());
    cases
}

fn core_evidence_cases() -> Vec<CaseEvidence> {
    vec![
        CaseEvidence {
            id: "owned_fixture_split_invariance",
            status: "pass",
            facts: json!({
                "every_split": true,
                "one_byte_reads": true,
                "explicit_expected_semantics": 10,
            }),
        },
        CaseEvidence {
            id: "seven_bit_and_c1_framing",
            status: "pass",
            facts: json!({"kitty_apc_7bit": true, "sixel_dcs_7bit": true, "sixel_dcs_c1": true}),
        },
        CaseEvidence {
            id: "can_sub_recovery",
            status: "pass",
            facts: json!({"kitty_can": true, "kitty_sub": true, "sixel_can": true, "sixel_sub": true}),
        },
        CaseEvidence {
            id: "malformed_and_unsupported",
            status: "pass",
            facts: json!({"typed_failures": 6, "mixed_terminator_rejected": true}),
        },
        CaseEvidence {
            id: "overlap_safe_termination",
            status: "pass",
            facts: json!({"repeated_escape_st": true, "escape_c1_st": true}),
        },
        CaseEvidence {
            id: "candidate_control_resynchronization",
            status: "pass",
            facts: json!({"apc": true, "dcs": true, "csi": true, "c1": true}),
        },
        CaseEvidence {
            id: "sixel_parameter_field_bounds",
            status: "pass",
            facts: json!({
                "fourth_field_rejected": true,
                "u16_overflow_rejected": true,
                "malformed_header_precedes_body_quota": true,
                "terminator_recovery_preserves_adjacent_text": true,
            }),
        },
        CaseEvidence {
            id: "truncated_sequence",
            status: "pass",
            facts: json!({"eof_rejected": true, "stream_reset": true}),
        },
        CaseEvidence {
            id: "control_string_exact_and_over_budget",
            status: "pass",
            facts: json!({"exact_bytes": 8, "over_budget_bytes": 9}),
        },
        CaseEvidence {
            id: "kitty_chunk_4096_and_max_plus_one",
            status: "pass",
            facts: json!({"max_payload_bytes": 4096, "max_accepted": true, "max_plus_one_rejected": true}),
        },
        CaseEvidence {
            id: "sixel_mode_parsing",
            status: "pass",
            facts: json!({"dec_sdm_7bit": true, "cursor_right_c1": true, "chronology_preserved": true}),
        },
        CaseEvidence {
            id: "raw_range_tiling",
            status: "pass",
            facts: json!({
                "every_owned_two_chunk_split": true,
                "every_owned_one_byte_feed": true,
                "forwarded_bytes_equal_source_slice": true,
            }),
        },
        CaseEvidence {
            id: "adjacent_text_preservation",
            status: "pass",
            facts: json!({"interrupted": true, "malformed": true, "unsupported": true, "over_budget": true}),
        },
    ]
}

fn hardening_evidence_cases() -> Vec<CaseEvidence> {
    vec![
        CaseEvidence {
            id: "sixel_header_max_plus_one_discard",
            status: "pass",
            facts: json!({
                "forms_verified": 2,
                "max_header_accepted": true,
                "q_at_boundary_accepted": true,
                "max_plus_one_numeric_rejected": true,
                "offending_byte_counted": true,
                "raw_payload_leaked": false,
                "exact_failure_ranges": true,
                "every_split": true,
            }),
        },
        CaseEvidence {
            id: "cancellation_preserves_first_failure",
            status: "pass",
            facts: json!({
                "malformed_header_can_forms": 2,
                "quota_sub_protocol_forms": 3,
                "cancellation_inclusive_ranges": true,
                "every_split": true,
            }),
        },
    ]
}

fn read_hex(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let hex = text.trim();
    if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid ASCII-hex fixture: {}", path.display()));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let high = decode_nibble(pair.first().copied())?;
        let low = decode_nibble(pair.get(1).copied())?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn decode_nibble(byte: Option<u8>) -> Result<u8, String> {
    match byte {
        Some(value @ b'0'..=b'9') => Ok(value.saturating_sub(b'0')),
        Some(value @ b'a'..=b'f') => Ok(value.saturating_sub(b'a').saturating_add(10)),
        Some(value @ b'A'..=b'F') => Ok(value.saturating_sub(b'A').saturating_add(10)),
        _ => Err("invalid hex nibble".to_owned()),
    }
}

fn assert_every_split(name: &str, bytes: &[u8]) -> Result<(), String> {
    let baseline_events = events_for_chunks(&[bytes], None);
    assert_ranges_tile(name, &baseline_events, bytes)?;
    let baseline = outcome_from_events(baseline_events);
    for split in 0..=bytes.len() {
        let left = bytes.get(..split).ok_or_else(|| "split left out of range".to_owned())?;
        let right = bytes.get(split..).ok_or_else(|| "split right out of range".to_owned())?;
        let events = events_for_chunks(&[left, right], None);
        assert_ranges_tile(name, &events, bytes)?;
        let actual = outcome_from_events(events);
        if actual != baseline {
            return Err(format!("{name}: framing changed at byte split {split}"));
        }
    }
    let one_byte_chunks = bytes.chunks(1).collect::<Vec<_>>();
    let events = events_for_chunks(&one_byte_chunks, None);
    assert_ranges_tile(name, &events, bytes)?;
    if outcome_from_events(events) != baseline {
        return Err(format!("{name}: framing changed with one-byte PTY reads"));
    }
    Ok(())
}

fn assert_owned_fixture_every_split(name: &str, bytes: &[u8]) -> Result<(), String> {
    for split in 0..=bytes.len() {
        let left = bytes.get(..split).ok_or_else(|| "split left out of range".to_owned())?;
        let right = bytes.get(split..).ok_or_else(|| "split right out of range".to_owned())?;
        let case = format!("{name} split {split}");
        let events = events_for_chunks(&[left, right], None);
        assert_ranges_tile(&case, &events, bytes)?;
        assert_owned_fixture_semantics(name, bytes, events)?;
    }

    let one_byte_chunks = bytes.chunks(1).collect::<Vec<_>>();
    let events = events_for_chunks(&one_byte_chunks, None);
    assert_ranges_tile(&format!("{name} one-byte feed"), &events, bytes)?;
    assert_owned_fixture_semantics(name, bytes, events)
}

fn observe(chunks: &[&[u8]], max: Option<usize>) -> Outcome {
    let events = events_for_chunks(chunks, max);
    let mut forwarded = Vec::new();
    let mut annotations = Vec::new();
    for event in events {
        if let Some(bytes) = event.terminal_bytes() {
            forwarded.extend_from_slice(bytes);
        }
        if !matches!(event, GraphicsEvent::Raw(_)) {
            annotations.push(event);
        }
    }
    Outcome { forwarded, annotations }
}

fn events_for_chunks(chunks: &[&[u8]], max: Option<usize>) -> Vec<GraphicsEvent> {
    let mut framer = validation_framer(max);
    let mut events = Vec::new();
    for chunk in chunks {
        match framer.push(chunk) {
            Ok(chunk_events) => events.extend(chunk_events),
            Err(_) => return Vec::new(),
        }
    }
    match framer.finish() {
        Ok(finish_events) => events.extend(finish_events),
        Err(_) => return Vec::new(),
    }
    events
}

fn assert_ranges_tile(name: &str, events: &[GraphicsEvent], input: &[u8]) -> Result<(), String> {
    let mut cursor = 0_u64;
    for event in events {
        let range = event.range();
        if range.start != cursor || range.end < range.start {
            return Err(format!("{name}: non-contiguous raw boundary {range:?} after {cursor}"));
        }
        if let Some(terminal_bytes) = event.terminal_bytes() {
            let start = usize::try_from(range.start)
                .map_err(|_| format!("{name}: range start does not fit usize: {range:?}"))?;
            let end = usize::try_from(range.end)
                .map_err(|_| format!("{name}: range end does not fit usize: {range:?}"))?;
            let range_len = end
                .checked_sub(start)
                .ok_or_else(|| format!("{name}: terminal-byte range underflow for {range:?}"))?;
            if range_len != terminal_bytes.len() {
                return Err(format!(
                    "{name}: terminal byte length {} does not match range length {range_len}",
                    terminal_bytes.len()
                ));
            }
            let source = input.get(start..end).ok_or_else(|| {
                format!("{name}: terminal-byte range lies outside input: {range:?}")
            })?;
            if terminal_bytes != source {
                return Err(format!("{name}: terminal bytes differ from source at {range:?}"));
            }
        }
        cursor = range.end;
    }
    if cursor != input.len() as u64 {
        return Err(format!("{name}: ranges end at {cursor}, expected {}", input.len()));
    }
    Ok(())
}

fn assert_owned_fixture_semantics(
    name: &str,
    bytes: &[u8],
    events: Vec<GraphicsEvent>,
) -> Result<(), String> {
    let outcome = outcome_from_events(events);
    let valid = match name {
        "kitty-query-order.hex" => matches_kitty_query(bytes, &outcome),
        "kitty-rgb-classic.hex" => matches_kitty_rgb(bytes, &outcome),
        "kitty-rgba-zlib-chunked.hex" => matches_kitty_chunked(bytes, &outcome),
        "kitty-png-classic.hex" => matches_kitty_png(bytes, &outcome),
        "kitty-unicode-placeholder.hex" => matches_kitty_placeholder(bytes, &outcome),
        "kitty-delete-lifecycle.hex" => matches_kitty_delete(bytes, &outcome),
        "sixel-7bit.hex" => matches_sixel_7bit(bytes, &outcome),
        "sixel-c1-transparent.hex" => matches_sixel_c1(bytes, &outcome),
        "sixel-mode-chronology.hex" => matches_sixel_chronology(bytes, &outcome),
        "malformed-recovery.hex" => matches_malformed_recovery(bytes, &outcome),
        _ => return Err(format!("owned fixture lacks an explicit expectation: {name}")),
    };
    if valid { Ok(()) } else { Err(format!("{name}: explicit semantic expectation failed")) }
}

fn matches_kitty_query(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(command_range) =
        consume_expected(bytes, &mut cursor, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")
    else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"\x1b[c").is_none() || cursor != bytes.len() {
        return false;
    }
    let mut expected = expected_kitty_command(KittyAction::Query, b"AAAA");
    expected.format = Some(KittyFormat::Rgb);
    expected.image_id = Some(31);
    expected.width = Some(1);
    expected.height = Some(1);
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Kitty { range, command, .. }]
            if *range == command_range
                && matches_kitty_command(command, &expected)
                && outcome.forwarded == b"\x1b[c"
    )
}

fn matches_kitty_rgb(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(command_range) =
        consume_expected(bytes, &mut cursor, b"\x1b_Ga=T,f=24,s=1,v=1,i=1;/wAA\x1b\\")
    else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let mut expected = expected_kitty_command(KittyAction::TransmitDisplay, b"/wAA");
    expected.format = Some(KittyFormat::Rgb);
    expected.image_id = Some(1);
    expected.width = Some(1);
    expected.height = Some(1);
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Kitty { range, command, .. }]
            if *range == command_range
                && matches_kitty_command(command, &expected)
                && outcome.forwarded.is_empty()
    )
}

fn matches_kitty_chunked(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(first_range) =
        consume_expected(bytes, &mut cursor, b"\x1b_Ga=T,f=32,s=1,v=1,i=2,o=z,m=1;eJz7z8DQ\x1b\\")
    else {
        return false;
    };
    let Some(second_range) = consume_expected(bytes, &mut cursor, b"\x1b_Gm=0;AAAEgAGA\x1b\\")
    else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let mut expected_first = expected_kitty_command(KittyAction::TransmitDisplay, b"eJz7z8DQ");
    expected_first.image_id = Some(2);
    expected_first.width = Some(1);
    expected_first.height = Some(1);
    expected_first.compression = KittyCompression::Zlib;
    expected_first.chunk_state = KittyChunkState::More;
    let expected_second = expected_kitty_command(KittyAction::Transmit, b"AAAEgAGA");
    matches!(
        outcome.annotations.as_slice(),
        [
            GraphicsEvent::Kitty { range: first_event_range, command: first, .. },
            GraphicsEvent::Kitty { range: second_event_range, command: second, .. },
        ] if *first_event_range == first_range
            && *second_event_range == second_range
            && matches_kitty_command(first, &expected_first)
            && matches_kitty_command(second, &expected_second)
            && outcome.forwarded.is_empty()
    )
}

fn matches_kitty_png(bytes: &[u8], outcome: &Outcome) -> bool {
    const PAYLOAD: &[u8] = b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg==";
    let mut cursor = 0;
    let mut expected_bytes = b"\x1b_Ga=T,f=100,i=3,c=1,r=1;".to_vec();
    expected_bytes.extend_from_slice(PAYLOAD);
    expected_bytes.extend_from_slice(b"\x1b\\");
    let Some(command_range) = consume_expected(bytes, &mut cursor, &expected_bytes) else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let mut expected = expected_kitty_command(KittyAction::TransmitDisplay, PAYLOAD);
    expected.format = Some(KittyFormat::Png);
    expected.image_id = Some(3);
    expected.columns = Some(1);
    expected.rows = Some(1);
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Kitty { range, command, .. }]
            if *range == command_range
                && matches_kitty_command(command, &expected)
                && outcome.forwarded.is_empty()
    )
}

fn matches_kitty_placeholder(bytes: &[u8], outcome: &Outcome) -> bool {
    const FORWARDED: &[u8] =
        b"\x1b[38;5;42m\xf4\x8e\xbb\xae\xcc\x85\xcc\x85\xf4\x8e\xbb\xae\xcc\x85\xcc\x8d\x1b[39m";
    let mut cursor = 0;
    let Some(command_range) = consume_expected(
        bytes,
        &mut cursor,
        b"\x1b_Ga=T,f=24,s=1,v=1,i=42,U=1,c=2,r=2,q=2;/wAA\x1b\\",
    ) else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, FORWARDED).is_none() || cursor != bytes.len() {
        return false;
    }
    let mut expected = expected_kitty_command(KittyAction::TransmitDisplay, b"/wAA");
    expected.format = Some(KittyFormat::Rgb);
    expected.image_id = Some(42);
    expected.width = Some(1);
    expected.height = Some(1);
    expected.columns = Some(2);
    expected.rows = Some(2);
    expected.placement_mode = KittyPlacementMode::UnicodePlaceholder;
    expected.quiet = 2;
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Kitty { range, command, .. }]
            if *range == command_range
                && matches_kitty_command(command, &expected)
                && outcome.forwarded == FORWARDED
    )
}

fn matches_kitty_delete(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(soft_range) = consume_expected(bytes, &mut cursor, b"\x1b_Ga=d,d=i,i=42,p=7\x1b\\")
    else {
        return false;
    };
    let Some(hard_range) = consume_expected(bytes, &mut cursor, b"\x1b_Ga=d,d=I,i=42\x1b\\") else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let mut expected_soft = expected_kitty_command(KittyAction::Delete, b"");
    expected_soft.image_id = Some(42);
    expected_soft.placement_id = Some(7);
    expected_soft.delete = Some(KittyDelete { selector: 'i', free_data: false });
    let mut expected_hard = expected_kitty_command(KittyAction::Delete, b"");
    expected_hard.image_id = Some(42);
    expected_hard.delete = Some(KittyDelete { selector: 'I', free_data: true });
    matches!(
        outcome.annotations.as_slice(),
        [
            GraphicsEvent::Kitty { range: soft_event_range, command: soft, .. },
            GraphicsEvent::Kitty { range: hard_event_range, command: hard, .. },
        ] if *soft_event_range == soft_range
            && *hard_event_range == hard_range
            && matches_kitty_command(soft, &expected_soft)
            && matches_kitty_command(hard, &expected_hard)
            && outcome.forwarded.is_empty()
    )
}

fn matches_sixel_7bit(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(command_range) = consume_expected(bytes, &mut cursor, bytes) else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let expected_parameters =
        SixelParameters { aspect: Some(0), background: Some(0), horizontal_grid: Some(0) };
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Sixel { range, command, .. }]
            if *range == command_range
                && command.parameters == expected_parameters
                && command.payload() == b"#0;2;100;0;0#0~"
                && outcome.forwarded.is_empty()
    )
}

fn matches_sixel_c1(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(command_range) = consume_expected(bytes, &mut cursor, bytes) else {
        return false;
    };
    if cursor != bytes.len() {
        return false;
    }
    let expected_parameters =
        SixelParameters { aspect: Some(0), background: Some(1), horizontal_grid: Some(0) };
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Sixel { range, command, .. }]
            if *range == command_range
                && command.parameters == expected_parameters
                && command.payload() == b"\"1;1;1;6#1;2;0;100;0#1~"
                && outcome.forwarded.is_empty()
    )
}

fn matches_sixel_chronology(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    let Some(display_off_range) = consume_expected(bytes, &mut cursor, b"\x1b[?80l") else {
        return false;
    };
    let Some(cursor_off_range) = consume_expected(bytes, &mut cursor, b"\x1b[?8452l") else {
        return false;
    };
    let Some(first_range) = consume_expected(bytes, &mut cursor, b"\x1bP0;0;0q~\x1b\\") else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"A").is_none() {
        return false;
    }
    let Some(cursor_on_range) = consume_expected(bytes, &mut cursor, b"\x1b[?8452h") else {
        return false;
    };
    let Some(second_range) = consume_expected(bytes, &mut cursor, b"\x1bP0;1;0q~\x1b\\") else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"B").is_none() {
        return false;
    }
    let Some(display_on_range) = consume_expected(bytes, &mut cursor, b"\x1b[?80h") else {
        return false;
    };
    let Some(third_range) = consume_expected(bytes, &mut cursor, b"\x1bP0;0;0q~\x1b\\") else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"C").is_none() || cursor != bytes.len() {
        return false;
    }
    let opaque = SixelParameters { aspect: Some(0), background: Some(0), horizontal_grid: Some(0) };
    let transparent =
        SixelParameters { aspect: Some(0), background: Some(1), horizontal_grid: Some(0) };
    matches!(
        outcome.annotations.as_slice(),
        [
            GraphicsEvent::SixelMode(display_off),
            GraphicsEvent::SixelMode(cursor_off),
            GraphicsEvent::Sixel { range: first_event_range, command: first, .. },
            GraphicsEvent::SixelMode(cursor_on),
            GraphicsEvent::Sixel { range: second_event_range, command: second, .. },
            GraphicsEvent::SixelMode(display_on),
            GraphicsEvent::Sixel { range: third_event_range, command: third, .. },
        ] if display_off.raw.range == display_off_range
            && display_off.raw.as_slice() == b"\x1b[?80l"
            && display_off.mode == SixelMode::Display && !display_off.enabled
            && cursor_off.raw.range == cursor_off_range
            && cursor_off.raw.as_slice() == b"\x1b[?8452l"
            && cursor_off.mode == SixelMode::CursorRight && !cursor_off.enabled
            && *first_event_range == first_range
            && first.parameters == opaque && first.payload() == b"~"
            && cursor_on.raw.range == cursor_on_range
            && cursor_on.raw.as_slice() == b"\x1b[?8452h"
            && cursor_on.mode == SixelMode::CursorRight && cursor_on.enabled
            && *second_event_range == second_range
            && second.parameters == transparent && second.payload() == b"~"
            && display_on.raw.range == display_on_range
            && display_on.raw.as_slice() == b"\x1b[?80h"
            && display_on.mode == SixelMode::Display && display_on.enabled
            && *third_event_range == third_range
            && third.parameters == opaque && third.payload() == b"~"
            && outcome.forwarded == b"\x1b[?80l\x1b[?8452lA\x1b[?8452hB\x1b[?80hC"
    )
}

fn matches_malformed_recovery(bytes: &[u8], outcome: &Outcome) -> bool {
    let mut cursor = 0;
    if consume_expected(bytes, &mut cursor, b"BEFORE").is_none() {
        return false;
    }
    let Some(kitty_range) =
        consume_expected(bytes, &mut cursor, b"\x1b_Ga=T,f=24,s=1,v=1,m=1;/wAA\x18")
    else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"AFTER").is_none() {
        return false;
    }
    let Some(sixel_range) = consume_expected(bytes, &mut cursor, b"\x1bP0;0;0q!999999999~\x1a")
    else {
        return false;
    };
    if consume_expected(bytes, &mut cursor, b"TAIL").is_none() || cursor != bytes.len() {
        return false;
    }
    matches!(
        outcome.annotations.as_slice(),
        [GraphicsEvent::Failure(kitty), GraphicsEvent::Failure(sixel)]
            if kitty.range == kitty_range
                && kitty.protocol == GraphicsProtocol::Kitty
                && kitty.category == GraphicsFailureCategory::MalformedFraming
                && kitty.limit.is_none()
                && sixel.range == sixel_range
                && sixel.protocol == GraphicsProtocol::Sixel
                && sixel.category == GraphicsFailureCategory::MalformedFraming
                && sixel.limit.is_none()
                && outcome.forwarded == b"BEFOREAFTERTAIL"
    )
}

struct ExpectedKittyCommand {
    action: KittyAction,
    format: Option<KittyFormat>,
    image_id: Option<u32>,
    placement_id: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    source_x: Option<u32>,
    source_y: Option<u32>,
    source_width: Option<u32>,
    source_height: Option<u32>,
    columns: Option<u32>,
    rows: Option<u32>,
    pixel_x: Option<u32>,
    pixel_y: Option<u32>,
    z_index: Option<i32>,
    move_cursor: Option<bool>,
    placement_mode: KittyPlacementMode,
    chunk_state: KittyChunkState,
    quiet: u8,
    compression: KittyCompression,
    delete: Option<KittyDelete>,
    payload: Vec<u8>,
}

fn expected_kitty_command(action: KittyAction, payload: &[u8]) -> ExpectedKittyCommand {
    ExpectedKittyCommand {
        action,
        format: Some(KittyFormat::Rgba),
        image_id: None,
        placement_id: None,
        width: None,
        height: None,
        source_x: None,
        source_y: None,
        source_width: None,
        source_height: None,
        columns: None,
        rows: None,
        pixel_x: None,
        pixel_y: None,
        z_index: None,
        move_cursor: None,
        placement_mode: KittyPlacementMode::Classic,
        chunk_state: KittyChunkState::Final,
        quiet: 0,
        compression: KittyCompression::None,
        delete: None,
        payload: payload.to_vec(),
    }
}

fn matches_kitty_command(command: &KittyCommand, expected: &ExpectedKittyCommand) -> bool {
    command.action == expected.action
        && command.format == expected.format
        && command.image_id == expected.image_id
        && command.placement_id == expected.placement_id
        && command.width == expected.width
        && command.height == expected.height
        && command.source_x == expected.source_x
        && command.source_y == expected.source_y
        && command.source_width == expected.source_width
        && command.source_height == expected.source_height
        && command.columns == expected.columns
        && command.rows == expected.rows
        && command.pixel_x == expected.pixel_x
        && command.pixel_y == expected.pixel_y
        && command.z_index == expected.z_index
        && command.move_cursor == expected.move_cursor
        && command.placement_mode == expected.placement_mode
        && command.chunk_state == expected.chunk_state
        && command.quiet == expected.quiet
        && command.compression == expected.compression
        && command.delete == expected.delete
        && command.payload() == expected.payload
}

fn consume_expected(input: &[u8], cursor: &mut usize, expected: &[u8]) -> Option<RawByteRange> {
    let start = *cursor;
    let end = start.checked_add(expected.len())?;
    if input.get(start..end)? != expected {
        return None;
    }
    *cursor = end;
    Some(RawByteRange { start: u64::try_from(start).ok()?, end: u64::try_from(end).ok()? })
}

fn verify_interruption_and_recovery() -> Result<(), String> {
    let cases = [
        ("Kitty CAN", b"HEAD\x1b_Ga=q;AAAA\x18TAIL".as_slice()),
        ("Kitty SUB", b"HEAD\x1b_Ga=q;AAAA\x1aTAIL".as_slice()),
        ("Sixel CAN", b"HEAD\x1bP0;0;0q~\x18TAIL".as_slice()),
        ("Sixel SUB", b"HEAD\x1bP0;0;0q~\x1aTAIL".as_slice()),
    ];
    for (name, bytes) in cases {
        assert_every_split(name, bytes)?;
        let outcome = observe(&[bytes], None);
        if outcome.forwarded != b"HEADTAIL"
            || !matches!(
                outcome.annotations.as_slice(),
                [GraphicsEvent::Failure(failure)]
                    if failure.category == GraphicsFailureCategory::MalformedFraming
            )
        {
            return Err(format!("{name}: interruption did not preserve adjacent text"));
        }
    }
    Ok(())
}

fn verify_malformed_and_unsupported() -> Result<(), String> {
    let cases = [
        (
            "duplicate Kitty control",
            b"PRE\x1b_Ga=q,a=q;AAAA\x1b\\POST".as_slice(),
            GraphicsFailureCategory::MalformedControl,
        ),
        (
            "unsupported Kitty action",
            b"PRE\x1b_Ga=f;AAAA\x1b\\POST".as_slice(),
            GraphicsFailureCategory::UnsupportedAction,
        ),
        (
            "unsupported Kitty transport",
            b"PRE\x1b_Ga=t,t=f;AAAA\x1b\\POST".as_slice(),
            GraphicsFailureCategory::UnsupportedTransport,
        ),
        (
            "malformed Sixel payload",
            b"PRE\x1bP0;0;0q!x\x1b\\POST".as_slice(),
            GraphicsFailureCategory::MalformedPayload,
        ),
        (
            "excluded C1 Kitty APC",
            b"PRE\x9fGa=q;AAAA\x9cPOST".as_slice(),
            GraphicsFailureCategory::UnsupportedProtocol,
        ),
        (
            "mixed Sixel terminator",
            b"PRE\x1bP0;0;0q~\x9cPOST".as_slice(),
            GraphicsFailureCategory::MalformedFraming,
        ),
    ];
    for (name, bytes, category) in cases {
        assert_every_split(name, bytes)?;
        let outcome = observe(&[bytes], None);
        if outcome.forwarded != b"PREPOST"
            || !matches!(
                outcome.annotations.as_slice(),
                [GraphicsEvent::Failure(failure)] if failure.category == category
            )
        {
            return Err(format!("{name}: wrong typed failure or adjacent text"));
        }
    }
    Ok(())
}

fn verify_overlap_safe_termination() -> Result<(), String> {
    let cases = [
        (
            "repeated ESC before Kitty ST",
            b"PRE\x1b_Ga=q;AAAA\x1b\x1b\\TAIL".as_slice(),
            GraphicsProtocol::Kitty,
        ),
        (
            "ESC before C1 ST in Kitty APC",
            b"PRE\x1b_Ga=q;AAAA\x1b\x9cTAIL".as_slice(),
            GraphicsProtocol::Kitty,
        ),
        (
            "ESC before C1 ST in C1 Sixel DCS",
            b"PRE\x900;0;0q~\x1b\x9cTAIL".as_slice(),
            GraphicsProtocol::Sixel,
        ),
    ];
    for (name, bytes, protocol) in cases {
        assert_every_split(name, bytes)?;
        let outcome = observe(&[bytes], None);
        let expected_end = bytes.len().saturating_sub(b"TAIL".len()) as u64;
        if outcome.forwarded != b"PRETAIL"
            || !matches!(
                outcome.annotations.as_slice(),
                [GraphicsEvent::Failure(failure)]
                    if failure.protocol == protocol
                        && failure.category == GraphicsFailureCategory::MalformedFraming
                        && failure.range == (RawByteRange { start: 3, end: expected_end })
            )
        {
            return Err(format!("{name}: overlapping terminator swallowed bytes or lost range"));
        }
    }
    Ok(())
}

fn verify_candidate_resynchronization() -> Result<(), String> {
    let escape_overlap = b"PRE\x1b\x1b_Ga=q;AAAA\x1b\\TAIL";
    assert_every_split("ESC overlap before Kitty APC", escape_overlap)?;
    let escape_outcome = observe(&[escape_overlap], None);
    if escape_outcome.forwarded != b"PRE\x1bTAIL"
        || !matches!(
            escape_outcome.annotations.as_slice(),
            [GraphicsEvent::Kitty { range, command, .. }]
                if range.start == 4 && command.action == KittyAction::Query
        )
    {
        return Err("ESC overlap did not reprocess the following Kitty APC".to_owned());
    }

    let apc_to_dcs = b"PRE\x1b_\x1bP0;0;0q~\x1b\\TAIL";
    assert_every_split("aborted APC prefix before Sixel DCS", apc_to_dcs)?;
    let apc_outcome = observe(&[apc_to_dcs], None);
    if apc_outcome.forwarded != b"PRE\x1b_TAIL"
        || !matches!(apc_outcome.annotations.as_slice(), [GraphicsEvent::Sixel { .. }])
    {
        return Err("aborted APC prefix swallowed the following Sixel DCS".to_owned());
    }

    let dcs_to_apc = b"PRE\x1bP0;\x1b_Ga=q;AAAA\x1b\\TAIL";
    assert_every_split("aborted DCS header before Kitty APC", dcs_to_apc)?;
    let dcs_outcome = observe(&[dcs_to_apc], None);
    if dcs_outcome.forwarded != b"PRE\x1bP0;TAIL"
        || !matches!(
            dcs_outcome.annotations.as_slice(),
            [GraphicsEvent::Kitty { command, .. }] if command.action == KittyAction::Query
        )
    {
        return Err("aborted DCS header swallowed the following Kitty APC".to_owned());
    }

    let csi_to_csi = b"PRE\x1b[?8\x1b[?80hTAIL";
    assert_every_split("aborted CSI prefix before Sixel mode", csi_to_csi)?;
    let csi_outcome = observe(&[csi_to_csi], None);
    if csi_outcome.forwarded != csi_to_csi
        || !matches!(
            csi_outcome.annotations.as_slice(),
            [GraphicsEvent::SixelMode(change)]
                if change.mode == SixelMode::Display && change.enabled
        )
    {
        return Err("aborted CSI prefix swallowed the following Sixel mode".to_owned());
    }

    let c1_apc_to_dcs = b"PRE\x9f\x900;0;0q~\x9cTAIL";
    assert_every_split("aborted C1 APC prefix before C1 Sixel", c1_apc_to_dcs)?;
    let c1_outcome = observe(&[c1_apc_to_dcs], None);
    if c1_outcome.forwarded != b"PRE\x9fTAIL"
        || !matches!(c1_outcome.annotations.as_slice(), [GraphicsEvent::Sixel { .. }])
    {
        return Err("aborted C1 APC prefix swallowed the following C1 Sixel DCS".to_owned());
    }
    Ok(())
}

fn verify_sixel_parameter_bounds() -> Result<(), String> {
    let fourth_field = b"PRE\x1bP0;0;0;0q~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\x1b\\POST";
    assert_every_split_with_max("fourth Sixel parameter field", fourth_field, 16)?;
    assert_malformed_sixel_control("fourth Sixel parameter field", fourth_field, Some(16))?;

    let overflow = b"PRE\x1bP65536;0;0q~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\x1b\\POST";
    assert_every_split_with_max("overflowing Sixel parameter", overflow, 16)?;
    assert_malformed_sixel_control("overflowing Sixel parameter", overflow, Some(16))?;

    let empty_fields = b"PRE\x1bP;;q~\x1b\\POST";
    assert_every_split("empty Sixel parameter fields", empty_fields)?;
    let empty_outcome = observe(&[empty_fields], None);
    if empty_outcome.forwarded != b"PREPOST"
        || !matches!(
            empty_outcome.annotations.as_slice(),
            [GraphicsEvent::Sixel { command, .. }]
                if command.parameters
                    == (SixelParameters {
                        aspect: None,
                        background: None,
                        horizontal_grid: None,
                    })
        )
    {
        return Err("empty Sixel fields did not preserve absent parameters".to_owned());
    }

    let huge_field = format!("PRE\x1bP{};0;0q~\x1b\\POST", "9".repeat(65_536));
    assert_malformed_sixel_control("huge Sixel parameter field", huge_field.as_bytes(), None)?;

    let repeated_fields = format!("PRE\x1bP{}q~\x1b\\POST", "0;".repeat(131_072));
    assert_malformed_sixel_control(
        "repeated Sixel parameter fields",
        repeated_fields.as_bytes(),
        None,
    )
}

fn assert_malformed_sixel_control(
    name: &str,
    bytes: &[u8],
    max: Option<usize>,
) -> Result<(), String> {
    let events = events_for_chunks(&[bytes], max);
    assert_ranges_tile(name, &events, bytes)?;
    let outcome = outcome_from_events(events);
    if outcome.forwarded != b"PREPOST"
        || !matches!(
            outcome.annotations.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.protocol == GraphicsProtocol::Sixel
                    && failure.category == GraphicsFailureCategory::MalformedControl
                    && failure.limit.is_none()
        )
    {
        return Err(format!("{name}: wrong typed rejection or adjacent text"));
    }
    Ok(())
}

fn verify_unterminated_recovery() -> Result<(), String> {
    let mut framer = validation_framer(None);
    let mut events = framer
        .push(b"PRE\x1b_Ga=q;AAAA")
        .map_err(|error| format!("unterminated push allocation: {error:?}"))?;
    events
        .try_extend(
            framer
                .finish()
                .map_err(|error| format!("unterminated finish allocation: {error:?}"))?,
        )
        .map_err(|error| format!("unterminated event growth: {error:?}"))?;
    let first = outcome_from_events(events);
    if first.forwarded != b"PRE"
        || !matches!(
            first.annotations.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.category == GraphicsFailureCategory::TruncatedSequence
        )
    {
        return Err("unterminated Kitty sequence was not rejected".to_owned());
    }
    let second = outcome_from_events(
        framer.push(b"AFTER").map_err(|error| format!("recovery output allocation: {error:?}"))?,
    );
    if second.forwarded != b"AFTER" || !second.annotations.is_empty() {
        return Err("text after an unterminated-stream reset was swallowed".to_owned());
    }
    Ok(())
}

fn verify_exact_and_over_budget() -> Result<(), String> {
    let exact = b"PRE\x1bP0;0;0q~~\x1b\\POST";
    let exact_outcome = observe(&[exact], Some(8));
    if exact_outcome.forwarded != b"PREPOST"
        || !matches!(exact_outcome.annotations.as_slice(), [GraphicsEvent::Sixel { .. }])
    {
        return Err("exact control-string ceiling was not accepted".to_owned());
    }

    let over = b"PRE\x1bP0;0;0q~~~\x1b\\POST";
    assert_every_split_with_max("over-budget Sixel", over, 8)?;
    let over_outcome = observe(&[over], Some(8));
    if over_outcome.forwarded != b"PREPOST"
        || !matches!(
            over_outcome.annotations.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.category == GraphicsFailureCategory::QuotaExceeded
                    && failure.limit == Some(GraphicsLimit::ControlString)
        )
    {
        return Err("over-budget Sixel did not recover at ST".to_owned());
    }

    let maximum_payload = format!("PRE\x1b_Ga=t,m=0;{}\x1b\\POST", "A".repeat(4_096));
    let maximum_outcome = observe(&[maximum_payload.as_bytes()], None);
    if maximum_outcome.forwarded != b"PREPOST"
        || !matches!(
            maximum_outcome.annotations.as_slice(),
            [GraphicsEvent::Kitty { command, .. }] if command.payload().len() == 4_096
        )
    {
        return Err("maximum Kitty chunk payload was not accepted".to_owned());
    }

    let oversized_payload = format!("PRE\x1b_Ga=t,m=0;{}\x1b\\POST", "A".repeat(4_097));
    let payload_outcome = observe(&[oversized_payload.as_bytes()], None);
    if payload_outcome.forwarded != b"PREPOST"
        || !matches!(
            payload_outcome.annotations.as_slice(),
            [GraphicsEvent::Failure(failure)]
                if failure.category == GraphicsFailureCategory::QuotaExceeded
                    && failure.limit == Some(GraphicsLimit::KittyChunkPayload)
        )
    {
        return Err("oversized Kitty chunk returned wrong typed limit".to_owned());
    }
    Ok(())
}

fn verify_sixel_header_quota_boundary() -> Result<(), String> {
    let cases = [
        ("7-bit Sixel q at header boundary", b"PRE\x1bP00q\x1b\\POST".as_slice(), 3, None),
        ("C1 Sixel q at header boundary", b"PRE\x9000q\x9cPOST".as_slice(), 3, None),
        (
            "7-bit Sixel max-plus-one numeric header",
            b"PRE\x1bP0000q~\x1b\\POST".as_slice(),
            3,
            Some(RawByteRange { start: 3, end: 13 }),
        ),
        (
            "C1 Sixel max-plus-one numeric header",
            b"PRE\x900000q~\x9cPOST".as_slice(),
            3,
            Some(RawByteRange { start: 3, end: 11 }),
        ),
        (
            "7-bit Sixel max-plus-one semicolon header",
            b"PRE\x1bP000;q~\x1b\\POST".as_slice(),
            3,
            Some(RawByteRange { start: 3, end: 13 }),
        ),
        (
            "C1 Sixel max-plus-one semicolon header",
            b"PRE\x90000;q~\x9cPOST".as_slice(),
            3,
            Some(RawByteRange { start: 3, end: 11 }),
        ),
    ];

    for (name, bytes, max, expected_failure_range) in cases {
        assert_every_split_with_max(name, bytes, max)?;
        let outcome = observe(&[bytes], Some(max));
        if outcome.forwarded != b"PREPOST" {
            return Err(format!("{name}: candidate or payload leaked to raw output"));
        }
        match expected_failure_range {
            None if matches!(
                outcome.annotations.as_slice(),
                [GraphicsEvent::Sixel { range, command, .. }]
                    if *range == (RawByteRange {
                        start: 3,
                        end: bytes.len().saturating_sub(4) as u64,
                    }) && command.payload().is_empty()
            ) => {}
            Some(expected)
                if matches!(
                    outcome.annotations.as_slice(),
                    [GraphicsEvent::Failure(failure)]
                        if failure.protocol == GraphicsProtocol::Sixel
                            && failure.category == GraphicsFailureCategory::QuotaExceeded
                            && failure.limit == Some(GraphicsLimit::ControlString)
                            && failure.range == expected
                ) => {}
            _ => {
                return Err(format!(
                    "{name}: boundary result or exact range was wrong: {:?}",
                    outcome.annotations
                ));
            }
        }
    }
    Ok(())
}

fn verify_cancellation_preserves_first_failure() -> Result<(), String> {
    let cases = [
        (
            "7-bit malformed Sixel header then CAN",
            b"PRE\x1bP0;0;0;0q~\x18POST".as_slice(),
            32,
            GraphicsFailureCategory::MalformedControl,
            None,
            RawByteRange { start: 3, end: 15 },
        ),
        (
            "C1 malformed Sixel header then CAN",
            b"PRE\x900;0;0;0q~\x18POST".as_slice(),
            32,
            GraphicsFailureCategory::MalformedControl,
            None,
            RawByteRange { start: 3, end: 14 },
        ),
        (
            "Kitty quota then SUB",
            b"PRE\x1b_Ga=q;A\x1aPOST".as_slice(),
            4,
            GraphicsFailureCategory::QuotaExceeded,
            Some(GraphicsLimit::ControlString),
            RawByteRange { start: 3, end: 12 },
        ),
        (
            "7-bit Sixel quota then SUB",
            b"PRE\x1bP0q~~~\x1aPOST".as_slice(),
            3,
            GraphicsFailureCategory::QuotaExceeded,
            Some(GraphicsLimit::ControlString),
            RawByteRange { start: 3, end: 11 },
        ),
        (
            "C1 Sixel quota then SUB",
            b"PRE\x900q~~~\x1aPOST".as_slice(),
            3,
            GraphicsFailureCategory::QuotaExceeded,
            Some(GraphicsLimit::ControlString),
            RawByteRange { start: 3, end: 10 },
        ),
    ];

    for (name, bytes, max, category, limit, range) in cases {
        assert_every_split_with_max(name, bytes, max)?;
        let outcome = observe(&[bytes], Some(max));
        if outcome.forwarded != b"PREPOST"
            || !matches!(
                outcome.annotations.as_slice(),
                [GraphicsEvent::Failure(failure)]
                    if failure.category == category
                        && failure.limit == limit
                        && failure.range == range
            )
        {
            return Err(format!("{name}: cancellation replaced first failure or lost its range"));
        }
    }
    Ok(())
}

fn assert_every_split_with_max(name: &str, bytes: &[u8], max: usize) -> Result<(), String> {
    let baseline_events = events_for_chunks(&[bytes], Some(max));
    assert_ranges_tile(name, &baseline_events, bytes)?;
    let baseline = outcome_from_events(baseline_events);
    for split in 0..=bytes.len() {
        let left = bytes.get(..split).ok_or_else(|| "split left out of range".to_owned())?;
        let right = bytes.get(split..).ok_or_else(|| "split right out of range".to_owned())?;
        let events = events_for_chunks(&[left, right], Some(max));
        assert_ranges_tile(name, &events, bytes)?;
        if outcome_from_events(events) != baseline {
            return Err(format!("{name}: changed at split {split}"));
        }
    }
    let one_byte_chunks = bytes.chunks(1).collect::<Vec<_>>();
    let events = events_for_chunks(&one_byte_chunks, Some(max));
    assert_ranges_tile(name, &events, bytes)?;
    if outcome_from_events(events) != baseline {
        return Err(format!("{name}: changed with one-byte PTY reads"));
    }
    Ok(())
}

fn verify_non_image_passthrough() -> Result<(), String> {
    let bytes = b"A\x1b]0;title\x07B\x1bP1;2pREGIS\x1b\\C\x1b[?2026hD\x1b[?2026lE";
    assert_every_split("non-image passthrough", bytes)?;
    let passthrough_outcome = observe(&[bytes], None);
    if passthrough_outcome.forwarded != bytes || !passthrough_outcome.annotations.is_empty() {
        return Err(format!("non-image bytes changed: {:?}", passthrough_outcome.forwarded));
    }

    let bounded_dcs = b"PRE\x1bP1;2;3pREGIS\x1b\\POST";
    assert_every_split_with_max("bounded non-Sixel DCS passthrough", bounded_dcs, 5)?;
    let bounded_outcome = observe(&[bounded_dcs], Some(5));
    if bounded_outcome.forwarded != bounded_dcs || !bounded_outcome.annotations.is_empty() {
        return Err("bounded DCS fallback changed non-Sixel bytes".to_owned());
    }

    let modes = b"\x1b[?80h\x9b?8452l";
    let mode_outcome = observe(&[modes], None);
    if mode_outcome.forwarded != modes
        || !matches!(
            mode_outcome.annotations.as_slice(),
            [GraphicsEvent::SixelMode(first), GraphicsEvent::SixelMode(second)]
                if first.mode == SixelMode::Display && first.enabled
                    && second.mode == SixelMode::CursorRight && !second.enabled
        )
    {
        return Err("7-bit/C1 Sixel modes were not annotated and forwarded".to_owned());
    }
    Ok(())
}

fn outcome_from_events(events: impl IntoIterator<Item = GraphicsEvent>) -> Outcome {
    let mut forwarded = Vec::new();
    let mut annotations = Vec::new();
    for event in events {
        if let Some(bytes) = event.terminal_bytes() {
            forwarded.extend_from_slice(bytes);
        }
        if !matches!(event, GraphicsEvent::Raw(_)) {
            annotations.push(event);
        }
    }
    Outcome { forwarded, annotations }
}
