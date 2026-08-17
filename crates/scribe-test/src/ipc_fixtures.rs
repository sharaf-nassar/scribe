//! Stable `MessagePack` fixtures for the bounded terminal-image IPC contract.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use scribe_common::ids::{SessionId, WindowId};
use scribe_common::protocol::{
    CiRunDelta, ClientMessage, REMOTE_PROTOCOL_VERSION, RemoteRefusal, ServerMessage,
};
use scribe_common::terminal_images::{
    BoundedImageBytes, CellExtent, ImageBoundError, ImageLimits, PixelRect, PlaceholderMetadata,
    RemoteProtocolMismatch, RemoteProtocolUpdateTarget, TerminalCellAnchor,
    TerminalImageCapabilities, TerminalImageCapabilityMismatch, TerminalImageCellClip,
    TerminalImageDataChunk, TerminalImageDefinition, TerminalImageGeneration, TerminalImageId,
    TerminalImageLiveMessage, TerminalImagePlacement, TerminalImagePlacementKind,
    TerminalImageProtocol, TerminalImageReplayMessage, TerminalImageUpdate, TerminalOutputSequence,
    TerminalPlacementId,
};
use serde::{Deserialize, Serialize};

const SESSION_ID: &str = "11111111-2222-4333-8444-555555555555";
const WINDOW_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

#[derive(Deserialize)]
struct FixtureManifest {
    schema_version: u32,
    remote_protocol_version: u32,
    messagepack_hex: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    remote_protocol_version: u32,
    verified_fixtures: usize,
    max_replay_chunk_bytes: u64,
    local_handshake: LocalHandshakeEvidence,
    remote_mismatch: RemoteMismatchEvidence,
    placement_validation: PlacementValidationEvidence,
    messagepack_hex: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct LocalHandshakeEvidence {
    old_local_handshake_defaults: bool,
    new_local_handshake_round_trip: bool,
}

#[derive(Serialize)]
struct RemoteMismatchEvidence {
    older_remote_updates_client: bool,
    newer_remote_updates_server: bool,
}

#[derive(Serialize)]
struct PlacementValidationEvidence {
    clipped_replay_round_trip: bool,
    legacy_none_omitted_and_defaulted: bool,
    malformed_replays_rejected: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum LegacyClientMessage {
    Hello { window_id: Option<WindowId>, clipboard_gating: bool, takeover: bool },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum LegacyServerMessage {
    Welcome {
        window_id: WindowId,
        other_windows: Vec<WindowId>,
        clipboard_gating: bool,
        participant_id: Option<u64>,
    },
}

pub fn verify(fixtures: &Path, output: &Path, dump: bool) -> Result<(), String> {
    let manifest_bytes =
        std::fs::read(fixtures).map_err(|error| format!("read {}: {error}", fixtures.display()))?;
    let manifest: FixtureManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("decode {}: {error}", fixtures.display()))?;
    if manifest.schema_version != 1 {
        return Err(format!("unsupported fixture schema {}", manifest.schema_version));
    }
    if manifest.remote_protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err(format!(
            "fixture remote version {} != binary {}",
            manifest.remote_protocol_version, REMOTE_PROTOCOL_VERSION
        ));
    }

    let encoded = encoded_fixtures()?;
    if !dump {
        if manifest.messagepack_hex.is_empty() {
            return Err("fixture manifest has no MessagePack records".to_owned());
        }
        if manifest.messagepack_hex != encoded {
            return Err(describe_fixture_drift(&manifest.messagepack_hex, &encoded));
        }
    }

    decode_fixtures(&encoded)?;
    verify_bounds()?;
    let placement_validation = verify_placement_validation(&encoded, &fixture_model()?)?;
    let (older_remote_updates_client, newer_remote_updates_server) = verify_remote_directions()?;

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        verified_fixtures: encoded.len(),
        max_replay_chunk_bytes: ImageLimits::V1.max_replay_chunk_bytes,
        local_handshake: LocalHandshakeEvidence {
            old_local_handshake_defaults: true,
            new_local_handshake_round_trip: true,
        },
        remote_mismatch: RemoteMismatchEvidence {
            older_remote_updates_client,
            newer_remote_updates_server,
        },
        placement_validation,
        messagepack_hex: &encoded,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(&evidence)
        .map_err(|error| format!("encode evidence: {error}"))?;
    std::fs::write(output, json).map_err(|error| format!("write {}: {error}", output.display()))?;
    Ok(())
}

struct FixtureModel {
    session_id: SessionId,
    window_id: WindowId,
    generation: TerminalImageGeneration,
    definition: TerminalImageDefinition,
    chunk: TerminalImageDataChunk,
    placement: TerminalImagePlacement,
    mismatch: TerminalImageCapabilityMismatch,
}

fn fixture_model() -> Result<FixtureModel, String> {
    let session_id = SessionId::from_str(SESSION_ID).map_err(|error| error.to_string())?;
    let window_id = WindowId::from_str(WINDOW_ID).map_err(|error| error.to_string())?;
    let generation = TerminalImageGeneration(7);
    let definition = TerminalImageDefinition::new(TerminalImageId(42), generation, 1, 1, true)
        .map_err(|error| error.to_string())?;
    let chunk = TerminalImageDataChunk {
        id: definition.id,
        generation,
        offset: 0,
        data: BoundedImageBytes::new(vec![255, 0, 0, 128]).map_err(|error| error.to_string())?,
        final_chunk: true,
    };
    chunk.validate(&definition).map_err(|error| error.to_string())?;
    let placement = TerminalImagePlacement {
        id: TerminalPlacementId(9),
        image_id: definition.id,
        generation,
        protocol: TerminalImageProtocol::Kitty,
        kind: TerminalImagePlacementKind::KittyClassic,
        anchor: TerminalCellAnchor { row: 2, column: 3 },
        source: PixelRect { x: 0, y: 0, width: 1, height: 1 },
        destination: CellExtent { columns: 1, rows: 1 },
        pixel_offset_x: 0,
        pixel_offset_y: 0,
        z_index: -1,
        scrolls_with_grid: true,
        move_cursor: false,
        cell_clip: None,
        placeholder: None,
    };
    let mismatch = TerminalImageCapabilityMismatch::new(
        TerminalImageCapabilities::V1,
        TerminalImageCapabilities::default(),
    )
    .ok_or_else(|| "expected capability mismatch".to_owned())?;

    Ok(FixtureModel { session_id, window_id, generation, definition, chunk, placement, mismatch })
}

fn encoded_fixtures() -> Result<BTreeMap<String, String>, String> {
    let model = fixture_model()?;
    let mut values = BTreeMap::new();
    insert_local_fixtures(&mut values, &model)?;
    insert_image_fixtures(&mut values, &model)?;
    insert_remote_fixtures(&mut values)?;
    Ok(values.into_iter().map(|(name, bytes)| (name, hex(&bytes))).collect())
}

fn insert_local_fixtures(
    values: &mut BTreeMap<String, Vec<u8>>,
    model: &FixtureModel,
) -> Result<(), String> {
    insert_named(
        values,
        "local_hello_old",
        &LegacyClientMessage::Hello { window_id: None, clipboard_gating: true, takeover: false },
    )?;
    insert_named(
        values,
        "local_hello_new",
        &ClientMessage::Hello {
            window_id: None,
            clipboard_gating: true,
            takeover: false,
            join_window: false,
            terminal_images: TerminalImageCapabilities::V1,
            ci_run_bar: false,
            pi_provider: false,
        },
    )?;
    insert_named(
        values,
        "ci_run_cleared",
        &ServerMessage::CiRunState {
            repo_root: PathBuf::from("/work/scribe"),
            delta: CiRunDelta::Cleared { head_sha: "head-a".into() },
        },
    )?;
    insert_named(
        values,
        "local_welcome_old",
        &LegacyServerMessage::Welcome {
            window_id: model.window_id,
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
        },
    )?;
    insert_named(
        values,
        "local_welcome_new",
        &ServerMessage::Welcome {
            window_id: model.window_id,
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: false,
            beads_write: false,
            pi_provider: false,
        },
    )
}

fn insert_image_fixtures(
    values: &mut BTreeMap<String, Vec<u8>>,
    model: &FixtureModel,
) -> Result<(), String> {
    insert_named(
        values,
        "live_definition_chunk",
        &ServerMessage::TerminalImageLive {
            session_id: model.session_id,
            message: TerminalImageLiveMessage::Update {
                generation: model.generation,
                sequence: TerminalOutputSequence(11),
                update: TerminalImageUpdate::DefinitionChunk { chunk: model.chunk.clone() },
            },
        },
    )?;
    insert_replay_fixtures(values, model)?;
    insert_named(
        values,
        "capability_mismatch",
        &ServerMessage::TerminalImageCapabilityMismatch {
            session_id: model.session_id,
            mismatch: model.mismatch,
        },
    )
}

fn insert_replay_fixtures(
    values: &mut BTreeMap<String, Vec<u8>>,
    model: &FixtureModel,
) -> Result<(), String> {
    insert_named(
        values,
        "replay_begin",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::Begin {
                generation: model.generation,
                after_sequence: TerminalOutputSequence(10),
                definition_count: 1,
                placement_count: 1,
                total_rgba_bytes: 4,
                // Legacy default: an omitted screen keeps the pinned bytes and
                // proves an older peer still decodes the record.
                active_screen: None,
            },
        },
    )?;
    insert_named(
        values,
        "replay_definition",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::Definition {
                generation: model.generation,
                definition: model.definition.clone(),
            },
        },
    )?;
    insert_named(
        values,
        "replay_chunk",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::DefinitionChunk {
                generation: model.generation,
                chunk: model.chunk.clone(),
            },
        },
    )?;
    insert_named(
        values,
        "replay_placement",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::Placement {
                generation: model.generation,
                placement: model.placement.clone(),
                screen: None,
            },
        },
    )?;
    let mut clipped = model.placement.clone();
    clipped.destination = CellExtent { columns: 3, rows: 2 };
    clipped.pixel_offset_x = 1;
    clipped.pixel_offset_y = 1;
    clipped.cell_clip = Some(TerminalImageCellClip { top: 2, left: 4, bottom: 4, right: 6 });
    insert_named(
        values,
        "replay_placement_clipped",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::Placement {
                generation: model.generation,
                placement: clipped,
                screen: None,
            },
        },
    )?;
    insert_named(
        values,
        "replay_commit",
        &ServerMessage::TerminalImageReplay {
            session_id: model.session_id,
            message: TerminalImageReplayMessage::Commit {
                generation: model.generation,
                through_sequence: TerminalOutputSequence(10),
            },
        },
    )?;
    Ok(())
}

fn insert_remote_fixtures(values: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let previous = REMOTE_PROTOCOL_VERSION.saturating_sub(1);
    for (name, client_version, server_version) in [
        ("remote_client_older", previous, REMOTE_PROTOCOL_VERSION),
        ("remote_server_older", REMOTE_PROTOCOL_VERSION, previous),
    ] {
        insert_named(
            values,
            name,
            &ServerMessage::RemoteHandshakeReply {
                accepted: false,
                refusal: Some(RemoteRefusal::IncompatibleVersion),
                server_remote_protocol_version: server_version,
                server_scribe_version: "fixture".to_owned(),
                version_mismatch: RemoteProtocolMismatch::between(client_version, server_version),
            },
        )?;
    }
    Ok(())
}

fn insert_named<T: Serialize>(
    values: &mut BTreeMap<String, Vec<u8>>,
    name: &str,
    value: &T,
) -> Result<(), String> {
    let bytes = rmp_serde::to_vec_named(value).map_err(|error| error.to_string())?;
    values.insert(name.to_owned(), bytes);
    Ok(())
}

fn decode_fixtures(fixtures: &BTreeMap<String, String>) -> Result<(), String> {
    for (name, encoded) in fixtures {
        let bytes = unhex(encoded)?;
        match name.as_str() {
            "local_hello_old" | "local_hello_new" => decode_client_fixture(name, &bytes)?,
            _ => decode_server_fixture(name, &bytes)?,
        }
    }
    Ok(())
}

fn decode_client_fixture(name: &str, bytes: &[u8]) -> Result<(), String> {
    let message: ClientMessage =
        rmp_serde::from_slice(bytes).map_err(|error| format!("decode {name}: {error}"))?;
    let ClientMessage::Hello { join_window, terminal_images, ci_run_bar, .. } = message else {
        return Err(format!("{name} decoded as wrong client message"));
    };
    if name.ends_with("old") && terminal_images != TerminalImageCapabilities::default() {
        return Err("old Hello did not default image capabilities".to_owned());
    }
    if name.ends_with("old") && join_window {
        return Err("old Hello did not default join intent".to_owned());
    }
    if name.ends_with("old") && ci_run_bar {
        return Err("old Hello did not default CI capability".to_owned());
    }
    let _: LegacyClientMessage =
        rmp_serde::from_slice(bytes).map_err(|error| format!("legacy decode {name}: {error}"))?;
    Ok(())
}

fn decode_server_fixture(name: &str, bytes: &[u8]) -> Result<(), String> {
    let message: ServerMessage =
        rmp_serde::from_slice(bytes).map_err(|error| format!("decode {name}: {error}"))?;
    if name == "local_welcome_old" {
        verify_old_welcome(&message)?;
    } else if name == "ci_run_cleared"
        && !matches!(
            &message,
            ServerMessage::CiRunState {
                repo_root,
                delta: CiRunDelta::Cleared { head_sha }
            } if repo_root == Path::new("/work/scribe") && head_sha == "head-a"
        )
    {
        return Err("CI clear fixture lost its head identity".to_owned());
    } else if let ServerMessage::TerminalImageReplay { message, .. } = &message {
        message.validate().map_err(|error| format!("validate {name}: {error}"))?;
    }
    if name == "local_welcome_new" {
        let _: LegacyServerMessage = rmp_serde::from_slice(bytes)
            .map_err(|error| format!("legacy decode {name}: {error}"))?;
    }
    Ok(())
}

fn verify_old_welcome(message: &ServerMessage) -> Result<(), String> {
    let ServerMessage::Welcome { terminal_images, .. } = message else {
        return Err("old Welcome decoded as wrong message".to_owned());
    };
    if *terminal_images != TerminalImageCapabilities::default() {
        return Err("old Welcome did not default image capabilities".to_owned());
    }
    Ok(())
}

fn verify_bounds() -> Result<(), String> {
    BoundedImageBytes::new(vec![0; BoundedImageBytes::MAX_LEN])
        .map_err(|error| format!("maximum chunk rejected: {error}"))?;
    if BoundedImageBytes::new(vec![0; BoundedImageBytes::MAX_LEN + 1]).is_ok() {
        return Err("maximum-plus-one image chunk was accepted".to_owned());
    }
    let oversized = TerminalImageReplayMessage::Begin {
        generation: TerminalImageGeneration(1),
        after_sequence: TerminalOutputSequence(0),
        definition_count: ImageLimits::V1.max_images_per_session + 1,
        placement_count: 0,
        total_rgba_bytes: 0,
        active_screen: None,
    };
    if oversized.validate().is_ok() {
        return Err("maximum-plus-one replay definition count was accepted".to_owned());
    }
    Ok(())
}

fn verify_placement_validation(
    fixtures: &BTreeMap<String, String>,
    model: &FixtureModel,
) -> Result<PlacementValidationEvidence, String> {
    let legacy_none_omitted_and_defaulted = verify_legacy_placement(fixtures)?;
    let clipped_replay_round_trip = verify_clipped_placement(fixtures)?;
    let malformed_replays_rejected = verify_malformed_placements(model)?;
    Ok(PlacementValidationEvidence {
        clipped_replay_round_trip,
        legacy_none_omitted_and_defaulted,
        malformed_replays_rejected,
    })
}

fn verify_legacy_placement(fixtures: &BTreeMap<String, String>) -> Result<bool, String> {
    let legacy = fixtures
        .get("replay_placement")
        .ok_or_else(|| "missing legacy replay placement".to_owned())?;
    let legacy_bytes = unhex(legacy)?;
    let legacy_none_omitted =
        !legacy_bytes.windows(b"cell_clip".len()).any(|window| window == b"cell_clip");
    let decoded: ServerMessage =
        rmp_serde::from_slice(&legacy_bytes).map_err(|error| error.to_string())?;
    Ok(legacy_none_omitted
        && matches!(
            decoded,
            ServerMessage::TerminalImageReplay {
                message: TerminalImageReplayMessage::Placement {
                    placement: TerminalImagePlacement { cell_clip: None, .. },
                    ..
                },
                ..
            }
        ))
}

fn verify_clipped_placement(fixtures: &BTreeMap<String, String>) -> Result<bool, String> {
    let clipped = fixtures
        .get("replay_placement_clipped")
        .ok_or_else(|| "missing clipped replay placement".to_owned())?;
    let clipped_message: ServerMessage =
        rmp_serde::from_slice(&unhex(clipped)?).map_err(|error| error.to_string())?;
    let clipped_replay_round_trip = matches!(
        &clipped_message,
        ServerMessage::TerminalImageReplay {
            message: TerminalImageReplayMessage::Placement {
                placement: TerminalImagePlacement {
                    cell_clip: Some(TerminalImageCellClip { top: 2, left: 4, bottom: 4, right: 6 }),
                    ..
                },
                ..
            },
            ..
        }
    );
    if let ServerMessage::TerminalImageReplay { message, .. } = clipped_message {
        message.validate().map_err(|error| error.to_string())?;
    } else {
        return Err("clipped replay decoded as wrong server message".to_owned());
    }
    Ok(clipped_replay_round_trip)
}

fn verify_malformed_placements(model: &FixtureModel) -> Result<usize, String> {
    let mut reversed = model.placement.clone();
    reversed.cell_clip = Some(TerminalImageCellClip { top: 4, left: 3, bottom: 2, right: 4 });
    let mut empty = model.placement.clone();
    empty.cell_clip = Some(TerminalImageCellClip { top: 2, left: 3, bottom: 2, right: 4 });
    let mut out_of_range = model.placement.clone();
    out_of_range.cell_clip =
        Some(TerminalImageCellClip { top: 2, left: 3, bottom: 3, right: 65_537 });
    let mut placeholder_with_clip = model.placement.clone();
    placeholder_with_clip.kind = TerminalImagePlacementKind::KittyUnicodePlaceholder;
    placeholder_with_clip.placeholder = Some(PlaceholderMetadata {
        image_identity_bits: 32,
        placement_id_in_underline: false,
        background_alpha: 255,
    });
    placeholder_with_clip.cell_clip =
        Some(TerminalImageCellClip { top: 2, left: 3, bottom: 3, right: 4 });
    let mut protocol_mismatch = model.placement.clone();
    protocol_mismatch.protocol = TerminalImageProtocol::Sixel;

    let cases = [
        (reversed, ImageBoundError::InvalidPlacementClip),
        (empty, ImageBoundError::InvalidPlacementClip),
        (out_of_range, ImageBoundError::InvalidPlacementClip),
        (placeholder_with_clip, ImageBoundError::InvalidPlacementClip),
        (protocol_mismatch, ImageBoundError::InvalidPlacementKind),
    ];
    let malformed_replays_rejected = cases
        .into_iter()
        .filter(|(placement, expected)| {
            TerminalImageReplayMessage::Placement {
                generation: model.generation,
                placement: placement.clone(),
                screen: None,
            }
            .validate()
                == Err(*expected)
        })
        .count();
    if malformed_replays_rejected != 5 {
        return Err(format!(
            "only {malformed_replays_rejected} malformed placements were rejected"
        ));
    }

    Ok(malformed_replays_rejected)
}

fn verify_remote_directions() -> Result<(bool, bool), String> {
    let previous = REMOTE_PROTOCOL_VERSION.saturating_sub(1);
    let old_client = RemoteProtocolMismatch::between(previous, REMOTE_PROTOCOL_VERSION)
        .ok_or_else(|| "old client mismatch missing".to_owned())?;
    let old_server = RemoteProtocolMismatch::between(REMOTE_PROTOCOL_VERSION, previous)
        .ok_or_else(|| "old server mismatch missing".to_owned())?;
    Ok((
        old_client.update == RemoteProtocolUpdateTarget::Client,
        old_server.update == RemoteProtocolUpdateTarget::Server,
    ))
}

fn describe_fixture_drift(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> String {
    let mut differences = Vec::new();
    for name in expected.keys().chain(actual.keys()) {
        if expected.get(name) != actual.get(name) && !differences.contains(name) {
            differences.push(name.clone());
        }
    }
    format!("MessagePack fixture drift: {}", differences.join(", "))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(hex_digit(byte >> 4));
        output.push(hex_digit(byte & 0x0f));
    }
    output
}

fn hex_digit(value: u8) -> char {
    if value < 10 { char::from(b'0' + value) } else { char::from(b'a' + value - 10) }
}

fn unhex(encoded: &str) -> Result<Vec<u8>, String> {
    if !encoded.len().is_multiple_of(2) {
        return Err("odd-length MessagePack hex".to_owned());
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).map_err(|error| error.to_string())?;
            u8::from_str_radix(text, 16).map_err(|error| error.to_string())
        })
        .collect()
}
