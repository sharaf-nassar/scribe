//! Production-path evidence for image state crossing a server upgrade.
//!
//! Every case pauses a real session mid-stream, exports through the production
//! handoff seam, installs the payload on a second session built from the same
//! shared process policy, and then resumes the exact bytes the first session
//! never got to read. The control for each case is the same fixture fed to one
//! session with no handoff at all: if the restored session's generation,
//! output cursor, canonical scene, framer offset, or published records differ
//! from that control by anything, the upgrade lost or duplicated something.

use std::collections::BTreeMap;
use std::path::Path;

use scribe_common::terminal_images::{
    ImageLimits, TerminalImageDefinition, TerminalImageGeneration, TerminalImagePlacement,
    TerminalImageReplayMessage, TerminalOutputSequence, TerminalScreenKind,
};
use scribe_server::handoff::{
    HandoffSession, HandoffState, handoff_state_version, handoff_version_accepted,
};
use scribe_server::terminal_image_handoff::{
    HandoffImageCounters, HandoffImageExport, MAX_HANDOFF_IMAGE_BYTES, SessionImageHandoff,
};
use scribe_server::terminal_image_replay::{ReplayInputs, plan_replay};
use serde::Serialize;

use crate::framing_probe::read_hex;
use crate::terminal_image_replay::maximum_scene;
use crate::terminal_image_replies_sharing::{Probe, definition_payload, write_probe_evidence};

/// Handoff version a payload declares once it carries image state.
const IMAGE_HANDOFF_VERSION: u32 = 7;

/// Newest version a server that predates image state understands.
const PRE_IMAGE_HANDOFF_VERSION: u32 = 6;

const RGB_CLASSIC_FIXTURE: &str = "kitty-rgb-classic.hex";
const SIXEL_7BIT_FIXTURE: &str = "sixel-7bit.hex";
const ZLIB_CHUNKED_FIXTURE: &str = "kitty-rgba-zlib-chunked.hex";

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    partial_apc: ResumeEvidence,
    partial_dcs: ResumeEvidence,
    kitty_accumulation: ResumeEvidence,
    max_scene: MaxSceneEvidence,
    compatibility: CompatibilityEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

/// One paused-and-resumed session measured against its no-handoff control.
#[derive(Serialize)]
struct ResumeEvidence {
    /// Bytes the sender consumed before reads paused.
    consumed_before_pause: u64,
    /// The framer was mid-control-string when it paused.
    framer_partial: bool,
    /// Protocol of the open string, once its introducer was recognized.
    partial_protocol: String,
    /// Stream bytes the open string had already consumed.
    held_bytes: u64,
    /// A chunked transfer was mid-accumulation.
    pending_transfer: bool,
    /// Normalized bytes that transfer had already accumulated.
    pending_decoded_bytes: usize,
    /// Stream offset the successor resumed from. Equals the pause offset, so
    /// no byte is re-read and none is skipped.
    resumed_from_offset: u64,
    /// Offset both the restored and the control session ended on.
    final_offset: u64,
    control_final_offset: u64,
    /// Canonical scene after the resume, and the control's.
    definitions: usize,
    placements: usize,
    control_definitions: usize,
    control_placements: usize,
    generation: u64,
    control_generation: u64,
    sequence: u64,
    control_sequence: u64,
    parity: ResumeParity,
}

/// Where the resumed session has to be indistinguishable from the control.
#[derive(Serialize)]
struct ResumeParity {
    /// Canonical definitions and placements are identical to the control's.
    scene_matches_control: bool,
    /// The records the resumed read published match the control's exactly.
    published_matches_control: bool,
}

/// What the largest admissible scene costs a handoff payload.
#[derive(Serialize)]
struct MaxSceneEvidence {
    definitions: u32,
    placements: u32,
    total_rgba_bytes: u64,
    handoff_image_ceiling_bytes: u64,
    chunks: u32,
    max_chunk_bytes: u64,
    chunk_ceiling_bytes: u64,
    /// Records outside the chunk ceiling. Must stay zero.
    oversized_records: u32,
    /// Records that failed their own receiver-side validation. Must stay zero.
    invalid_records: u32,
    /// A max scene fits the payload ceiling exactly.
    fits_ceiling: bool,
    /// A session whose scene does not fit exports an empty scene, never a
    /// truncated one, and still carries its cursor and framing.
    dropped_scenes: u32,
    dropped_session_records: usize,
    dropped_session_definitions: usize,
    dropped_session_kept_framing: bool,
    refusals: MalformedRefusals,
}

/// Bursts a receiver has to refuse instead of staging half of.
#[derive(Serialize)]
struct MalformedRefusals {
    /// Restoring a burst whose `Begin` counts disagree with its records is
    /// refused outright.
    truncated_payload_refused: bool,
    /// Restoring a burst with a placement whose definition never arrived is
    /// refused outright.
    unbacked_placement_refused: bool,
}

/// Old/new and new/old behaviour of the handoff payload itself.
#[derive(Serialize)]
struct CompatibilityEvidence {
    /// Version an image-carrying payload declares.
    image_payload_version: u32,
    /// Version an image-free payload declares.
    image_free_payload_version: u32,
    /// With images disabled, nothing is exported at all.
    downgrade_exports_nothing: bool,
    downgrade_counters: HandoffImageCounters,
    old_to_new: OldToNewEvidence,
    rollback: RollbackEvidence,
    current_receiver: CurrentReceiverEvidence,
}

/// A payload from a server that predates image state.
#[derive(Serialize)]
struct OldToNewEvidence {
    /// An image-free payload omits the key entirely, so its bytes are the
    /// bytes a pre-image server produced.
    image_free_payload_omits_key: bool,
    /// Such a payload decodes with an absent scene rather than failing, and
    /// restores as an empty scene.
    old_to_new_restores_empty: bool,
}

/// What a server that predates image state does with a current payload.
#[derive(Serialize)]
struct RollbackEvidence {
    /// It refuses the image-carrying payload instead of silently dropping
    /// every session's images.
    new_to_old_refused: bool,
    /// It accepts the image-free payload, which is what makes rollback a
    /// config change rather than a cold restart.
    downgraded_payload_accepted: bool,
}

/// A current receiver still accepts both versions.
#[derive(Serialize)]
struct CurrentReceiverEvidence {
    current_accepts_image_payload: bool,
    current_accepts_pre_image_payload: bool,
}

pub fn run(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();

    let partial_apc = verify_resume(fixtures, RGB_CLASSIC_FIXTURE, PauseAt::BeforeTerminator)?;
    require(partial_apc.framer_partial, "the APC pause did not hold a partial string")?;
    require(partial_apc.partial_protocol == "kitty", "the APC pause held a non-Kitty string")?;
    cases.insert("partial_apc_resumes", "pass");

    let partial_dcs = verify_resume(fixtures, SIXEL_7BIT_FIXTURE, PauseAt::BeforeTerminator)?;
    require(partial_dcs.framer_partial, "the DCS pause did not hold a partial string")?;
    require(partial_dcs.partial_protocol == "sixel", "the DCS pause held a non-Sixel string")?;
    cases.insert("partial_dcs_resumes", "pass");

    let kitty_accumulation = verify_resume(fixtures, ZLIB_CHUNKED_FIXTURE, PauseAt::BetweenChunks)?;
    require(
        kitty_accumulation.pending_transfer,
        "the chunked pause carried no in-flight transfer",
    )?;
    cases.insert("kitty_chunk_accumulation_resumes", "pass");
    cases.insert("ordered_resume_without_loss", "pass");

    let max_scene = verify_max_scene(fixtures)?;
    cases.insert("max_scene_stays_bounded", "pass");
    cases.insert("oversized_scene_drops_whole", "pass");
    cases.insert("truncated_payload_refused", "pass");

    let compatibility = verify_compatibility(fixtures)?;
    cases.insert("old_to_new_restore", "pass");
    cases.insert("new_to_old_rollback_refusal", "pass");
    cases.insert("downgrade_config", "pass");

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server terminal image handoff",
        payload_free: true,
        partial_apc,
        partial_dcs,
        kitty_accumulation,
        max_scene,
        compatibility,
        cases,
    };
    write_probe_evidence(evidence_path, &evidence)
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition { Ok(()) } else { Err(message.to_owned()) }
}

/// Drive one PTY read all the way through commit and publication, which is
/// what leaves a canonical scene behind for the handoff to carry.
fn read_and_commit(probe: &mut Probe, bytes: &[u8]) -> Result<(), String> {
    let commit = probe.feed(bytes)?;
    probe
        .images
        .commit_and_publish(&commit, &mut definition_payload)
        .map(|_| ())
        .map_err(|error| format!("commit a read: {error}"))
}

/// Where the sender's last read stopped.
#[derive(Clone, Copy)]
enum PauseAt {
    /// Inside the control string, before its terminator: the framer holds a
    /// partial APC or DCS.
    BeforeTerminator,
    /// On a complete command boundary partway through a chunked transfer: the
    /// framer is in Ground but a transfer is mid-accumulation.
    BetweenChunks,
}

/// Split one fixture at the chosen pause point.
fn split_fixture(bytes: &[u8], pause: PauseAt) -> Result<(&[u8], &[u8]), String> {
    let terminators: Vec<usize> = (0..bytes.len().saturating_sub(1))
        .filter(|index| bytes.get(*index) == Some(&0x1b) && bytes.get(index + 1) == Some(&b'\\'))
        .collect();
    let split = match pause {
        // Just before the string terminator, so the framer is still Active.
        PauseAt::BeforeTerminator => *terminators
            .last()
            .ok_or_else(|| "fixture has no string terminator to pause before".to_owned())?,
        // Just past the first command's terminator, so the command completed
        // but the transfer it opened has not.
        PauseAt::BetweenChunks => terminators
            .first()
            .ok_or_else(|| "fixture has no first command to pause after".to_owned())?
            .saturating_add(2),
    };
    let head = bytes.get(..split).ok_or_else(|| "pause point out of range".to_owned())?;
    let tail = bytes.get(split..).ok_or_else(|| "resume point out of range".to_owned())?;
    if tail.is_empty() {
        return Err("pause point left nothing to resume".to_owned());
    }
    Ok((head, tail))
}

/// Pause one session mid-fixture, hand its state to a successor, and prove the
/// successor's resumed read is indistinguishable from never having paused.
// @lat: [[test#Test Harness#Terminal Image Handoff#Ordered Resume Without Loss]]
fn verify_resume(fixtures: &Path, fixture: &str, pause: PauseAt) -> Result<ResumeEvidence, String> {
    let bytes = read_hex(&fixtures.join(fixture))?;
    let (head, tail) = split_fixture(&bytes, pause)?;

    // Control: one session reads both halves with no upgrade in between.
    // Both sides commit every read, exactly as the production reader does.
    let mut control = Probe::new();
    read_and_commit(&mut control, head)?;
    let control_commit = control.feed(tail)?;
    let control_published = control
        .images
        .commit_and_publish(&control_commit, &mut definition_payload)
        .map_err(|error| format!("control publish: {error}"))?;
    let control_state = control.images.state();
    let control_export = control.images.export_handoff(&mut |_| None);

    // Sender: the same first read, then reads pause for the upgrade.
    let mut sender = Probe::new();
    read_and_commit(&mut sender, head)?;
    let mut sender_export = HandoffImageExport::new(true);
    let paused = sender_export
        .session(sender.images.session(), &mut definition_payload)
        .ok_or_else(|| "an enabled export produced no image state".to_owned())?;
    let counters = sender_export.counters();
    let pending_decoded_bytes = paused
        .pending_kitty
        .as_ref()
        .and_then(|pending| pending.transfer.decoded.as_ref())
        .map_or(0, Vec::len);

    // The payload crosses the upgrade socket, so measure it after a real
    // round trip rather than by handing the successor the sender's value.
    let payload = round_trip_session(&paused)?;

    // Receiver: a fresh session stages the payload before any byte is read.
    let mut receiver = Probe::new();
    let mut restored_pixels: Vec<(TerminalImageDefinition, usize)> = Vec::new();
    receiver
        .images
        .restore_handoff(&payload, &mut |definition, rgba| {
            restored_pixels.push((definition.clone(), rgba.len()));
        })
        .map_err(|error| format!("restore the handoff payload: {error}"))?;
    for (definition, len) in &restored_pixels {
        if *len as u64 != definition.rgba_bytes {
            return Err("a restored definition arrived with the wrong pixel count".to_owned());
        }
    }
    let resumed_commit = receiver.feed(tail)?;
    let published = receiver
        .images
        .commit_and_publish(&resumed_commit, &mut definition_payload)
        .map_err(|error| format!("resumed publish: {error}"))?;
    let state = receiver.images.state();
    let export_after = receiver.images.export_handoff(&mut |_| None);

    let scene_matches_control = receiver.images.canonical_definitions()
        == control.images.canonical_definitions()
        && receiver.images.canonical_placements() == control.images.canonical_placements();
    let published_matches_control = format!("{published:?}") == format!("{control_published:?}");

    let evidence = ResumeEvidence {
        consumed_before_pause: payload.framing.offset(),
        framer_partial: payload.framing.is_partial(),
        partial_protocol: payload
            .framing
            .protocol()
            .map_or_else(|| "none".to_owned(), |protocol| format!("{protocol:?}").to_lowercase()),
        held_bytes: payload.framing.held_bytes(),
        pending_transfer: payload.pending_kitty.is_some(),
        pending_decoded_bytes,
        resumed_from_offset: payload.framing.offset(),
        final_offset: export_after.state.framing.offset(),
        control_final_offset: control_export.state.framing.offset(),
        definitions: state.definition_count,
        placements: state.placement_count,
        control_definitions: control_state.definition_count,
        control_placements: control_state.placement_count,
        generation: state.generation.0,
        control_generation: control_state.generation.0,
        sequence: state.sequence.0,
        control_sequence: control_state.sequence.0,
        parity: ResumeParity { scene_matches_control, published_matches_control },
    };
    check_resume(&evidence, head.len() as u64, counters)?;
    Ok(evidence)
}

/// Everything a resumed session owes its control.
fn check_resume(
    evidence: &ResumeEvidence,
    paused_after: u64,
    counters: HandoffImageCounters,
) -> Result<(), String> {
    if evidence.consumed_before_pause != paused_after {
        return Err(format!(
            "paused after {paused_after} bytes but the payload said {}",
            evidence.consumed_before_pause
        ));
    }
    if evidence.final_offset != evidence.control_final_offset {
        return Err(format!(
            "resumed stream ended on offset {} against the control's {}",
            evidence.final_offset, evidence.control_final_offset
        ));
    }
    if evidence.generation != evidence.control_generation
        || evidence.sequence != evidence.control_sequence
    {
        return Err("the resumed session diverged from the control's cursor".to_owned());
    }
    if !evidence.parity.scene_matches_control {
        return Err("the resumed canonical scene differs from the control's".to_owned());
    }
    if !evidence.parity.published_matches_control {
        return Err("the resumed read published different records than the control".to_owned());
    }
    if counters.sessions != 1 || counters.dropped_scenes != 0 {
        return Err("one enabled session did not export exactly one intact scene".to_owned());
    }
    Ok(())
}

/// Encode and decode one payload the way the upgrade socket does.
fn round_trip_session(state: &SessionImageHandoff) -> Result<SessionImageHandoff, String> {
    let bytes =
        rmp_serde::to_vec_named(state).map_err(|error| format!("encode image handoff: {error}"))?;
    rmp_serde::from_slice(&bytes).map_err(|error| format!("decode image handoff: {error}"))
}

/// The largest scene v1 admits has to fit the payload ceiling in wire-sized
/// chunks, and a scene that does not fit must be dropped whole.
// @lat: [[test#Test Harness#Terminal Image Handoff#Maximum Scene and Payload Ceiling]]
fn verify_max_scene(fixtures: &Path) -> Result<MaxSceneEvidence, String> {
    let limits = ImageLimits::V1;
    let generation = TerminalImageGeneration(1);
    let definitions = maximum_scene(generation)?;
    let plan = plan_replay(
        &ReplayInputs {
            generation,
            through_sequence: TerminalOutputSequence(1),
            active_screen: TerminalScreenKind::Primary,
            definitions: &definitions,
            placements: &[],
        },
        &mut definition_payload,
    );
    let mut oversized = 0u32;
    let mut invalid = 0u32;
    for record in &plan.records {
        if record.validate().is_err() {
            invalid += 1;
        }
        if let TerminalImageReplayMessage::DefinitionChunk { chunk, .. } = record
            && chunk.data.len() as u64 > limits.max_replay_chunk_bytes
        {
            oversized += 1;
        }
    }
    let counters = plan.counters;
    drop(plan);

    // A real committed scene against a ceiling too small for it: the session
    // still travels, its scene does not, and nothing partial is emitted.
    let mut probe = Probe::new();
    read_and_commit(&mut probe, &read_hex(&fixtures.join(RGB_CLASSIC_FIXTURE))?)?;
    let mut exporter = HandoffImageExport::with_ceiling_for_validation(true, 1);
    let dropped = exporter
        .session(probe.images.session(), &mut definition_payload)
        .ok_or_else(|| "an enabled export produced no image state".to_owned())?;
    let dropped_counters = exporter.counters();
    let dropped_session_definitions = dropped
        .records
        .iter()
        .filter(|record| matches!(record, TerminalImageReplayMessage::Definition { .. }))
        .count();

    let evidence = MaxSceneEvidence {
        definitions: counters.definitions,
        placements: counters.placements,
        total_rgba_bytes: counters.total_rgba_bytes,
        handoff_image_ceiling_bytes: MAX_HANDOFF_IMAGE_BYTES,
        chunks: counters.chunks,
        max_chunk_bytes: counters.max_chunk_bytes,
        chunk_ceiling_bytes: limits.max_replay_chunk_bytes,
        oversized_records: oversized,
        invalid_records: invalid,
        fits_ceiling: counters.total_rgba_bytes <= MAX_HANDOFF_IMAGE_BYTES,
        dropped_scenes: dropped_counters.dropped_scenes,
        dropped_session_records: dropped.records.len(),
        dropped_session_definitions,
        dropped_session_kept_framing: dropped.generation == probe.images.state().generation
            && dropped.sequence == probe.images.state().sequence,
        refusals: MalformedRefusals {
            truncated_payload_refused: refuses_truncated_payload(&mut probe),
            unbacked_placement_refused: refuses_unbacked_placement()?,
        },
    };
    check_max_scene(&evidence)?;
    Ok(evidence)
}

fn check_max_scene(evidence: &MaxSceneEvidence) -> Result<(), String> {
    if evidence.oversized_records != 0 || evidence.invalid_records != 0 {
        return Err("the maximum scene produced an unusable handoff record".to_owned());
    }
    if !evidence.fits_ceiling {
        return Err("the maximum scene does not fit the handoff image ceiling".to_owned());
    }
    if evidence.max_chunk_bytes > evidence.chunk_ceiling_bytes {
        return Err("a handoff chunk exceeded the wire chunk ceiling".to_owned());
    }
    let expected = evidence.total_rgba_bytes.div_ceil(evidence.chunk_ceiling_bytes);
    if u64::from(evidence.chunks) != expected {
        return Err(format!(
            "the maximum scene planned {} chunks, expected {expected}",
            evidence.chunks
        ));
    }
    if evidence.dropped_scenes != 1 {
        return Err("the oversized scene was not dropped".to_owned());
    }
    // Begin and Commit and nothing between: an empty scene, not a partial one.
    if evidence.dropped_session_records != 2 || evidence.dropped_session_definitions != 0 {
        return Err("the dropped scene emitted a partial burst".to_owned());
    }
    if !evidence.dropped_session_kept_framing {
        return Err("the dropped scene took the session's cursor with it".to_owned());
    }
    if !evidence.refusals.truncated_payload_refused || !evidence.refusals.unbacked_placement_refused
    {
        return Err("a malformed handoff burst was accepted".to_owned());
    }
    Ok(())
}

/// A burst whose `Begin` promises more than its records deliver is truncated,
/// which is exactly what a partial scene would look like on the wire.
fn refuses_truncated_payload(probe: &mut Probe) -> bool {
    let mut exported = probe.images.export_handoff(&mut definition_payload).state;
    exported
        .records
        .retain(|record| !matches!(record, TerminalImageReplayMessage::DefinitionChunk { .. }));
    let mut receiver = Probe::new();
    receiver.images.restore_handoff(&exported, &mut |_, _| {}).is_err()
}

/// A placement naming a definition the burst never carried would leave the
/// successor painting a hole.
fn refuses_unbacked_placement() -> Result<bool, String> {
    let generation = TerminalImageGeneration(2);
    let definition = TerminalImageDefinition::new(
        scribe_common::terminal_images::TerminalImageId(9),
        generation,
        2,
        2,
        false,
    )
    .map_err(|error| format!("build an unbacked definition: {error}"))?;
    let placements: Vec<(TerminalScreenKind, TerminalImagePlacement)> = Vec::new();
    let plan = plan_replay(
        &ReplayInputs {
            generation,
            through_sequence: TerminalOutputSequence(3),
            active_screen: TerminalScreenKind::Primary,
            definitions: std::slice::from_ref(&definition),
            placements: &placements,
        },
        &mut definition_payload,
    );
    let mut records = plan.records;
    // Drop the definition and its pixels, keep a placement that names it.
    records.retain(|record| {
        matches!(
            record,
            TerminalImageReplayMessage::Begin { .. } | TerminalImageReplayMessage::Commit { .. }
        )
    });
    let placement = TerminalImagePlacement {
        id: scribe_common::terminal_images::TerminalPlacementId(1),
        image_id: definition.id,
        generation,
        protocol: scribe_common::terminal_images::TerminalImageProtocol::Kitty,
        kind: scribe_common::terminal_images::TerminalImagePlacementKind::KittyClassic,
        anchor: scribe_common::terminal_images::TerminalCellAnchor { row: 0, column: 0 },
        source: scribe_common::terminal_images::PixelRect { x: 0, y: 0, width: 2, height: 2 },
        destination: scribe_common::terminal_images::CellExtent { columns: 1, rows: 1 },
        pixel_offset_x: 0,
        pixel_offset_y: 0,
        z_index: 0,
        scrolls_with_grid: true,
        move_cursor: false,
        cell_clip: None,
        placeholder: None,
    };
    placement.validate_scalars().map_err(|error| format!("unbacked placement: {error}"))?;
    let commit = records.pop().ok_or_else(|| "burst lost its commit".to_owned())?;
    records.push(TerminalImageReplayMessage::Placement {
        generation,
        placement,
        screen: Some(TerminalScreenKind::Primary),
    });
    records.push(commit);
    let payload = SessionImageHandoff {
        generation,
        sequence: TerminalOutputSequence(3),
        active_screen: TerminalScreenKind::Primary,
        published_screen: TerminalScreenKind::Primary,
        next_assigned_image_id: u64::from(u32::MAX) + 1,
        records,
        framing: Probe::new().images.export_handoff(&mut |_| None).state.framing,
        pending_kitty: None,
    };
    let mut receiver = Probe::new();
    Ok(receiver.images.restore_handoff(&payload, &mut |_, _| {}).is_err())
}

/// Old-to-new restore, new-to-old refusal, and the downgrade that makes
/// rollback a config change instead of a cold restart.
// @lat: [[test#Test Harness#Terminal Image Handoff#Version Compatibility and Downgrade]]
fn verify_compatibility(fixtures: &Path) -> Result<CompatibilityEvidence, String> {
    let mut probe = Probe::new();
    read_and_commit(&mut probe, &read_hex(&fixtures.join(RGB_CLASSIC_FIXTURE))?)?;

    let mut enabled = HandoffImageExport::new(true);
    let image_state = enabled
        .session(probe.images.session(), &mut definition_payload)
        .ok_or_else(|| "an enabled export produced no image state".to_owned())?;

    let mut disabled = HandoffImageExport::new(false);
    let downgraded = disabled.session(probe.images.session(), &mut definition_payload);
    let downgrade_counters = disabled.counters();

    let with_images = vec![session_stub(Some(image_state))];
    let without_images = vec![session_stub(downgraded)];
    let image_payload_version = handoff_state_version(&with_images);
    let image_free_payload_version = handoff_state_version(&without_images);

    let image_free_bytes = encode_state(without_images, image_free_payload_version)?;
    let image_free_payload_omits_key =
        !image_free_bytes.windows(b"image_state".len()).any(|window| window == b"image_state");
    let decoded: HandoffState = rmp_serde::from_slice(&image_free_bytes)
        .map_err(|error| format!("decode a pre-image payload: {error}"))?;
    let old_to_new_restores_empty =
        decoded.sessions.first().is_some_and(|session| session.image_state.is_none())
            && decoded.version == PRE_IMAGE_HANDOFF_VERSION;

    let image_bytes = encode_state(with_images, image_payload_version)?;
    let image_decoded: HandoffState = rmp_serde::from_slice(&image_bytes)
        .map_err(|error| format!("decode an image payload: {error}"))?;
    if image_decoded.sessions.first().and_then(|session| session.image_state.as_ref()).is_none() {
        return Err("an image payload lost its scene on the wire".to_owned());
    }

    let evidence = CompatibilityEvidence {
        image_payload_version,
        image_free_payload_version,
        downgrade_exports_nothing: downgrade_counters == HandoffImageCounters::default(),
        downgrade_counters,
        old_to_new: OldToNewEvidence { image_free_payload_omits_key, old_to_new_restores_empty },
        rollback: RollbackEvidence {
            new_to_old_refused: !handoff_version_accepted(
                image_payload_version,
                PRE_IMAGE_HANDOFF_VERSION,
            ),
            downgraded_payload_accepted: handoff_version_accepted(
                image_free_payload_version,
                PRE_IMAGE_HANDOFF_VERSION,
            ),
        },
        current_receiver: CurrentReceiverEvidence {
            current_accepts_image_payload: handoff_version_accepted(
                image_payload_version,
                IMAGE_HANDOFF_VERSION,
            ),
            current_accepts_pre_image_payload: handoff_version_accepted(
                image_free_payload_version,
                IMAGE_HANDOFF_VERSION,
            ),
        },
    };
    check_compatibility(&evidence)?;
    Ok(evidence)
}

fn check_compatibility(evidence: &CompatibilityEvidence) -> Result<(), String> {
    if evidence.image_payload_version != IMAGE_HANDOFF_VERSION {
        return Err("an image-carrying payload did not declare the image version".to_owned());
    }
    if evidence.image_free_payload_version != PRE_IMAGE_HANDOFF_VERSION {
        return Err("an image-free payload did not declare the pre-image version".to_owned());
    }
    if !evidence.old_to_new.image_free_payload_omits_key {
        return Err("an image-free payload still carried the image key".to_owned());
    }
    if !evidence.old_to_new.old_to_new_restores_empty {
        return Err("a pre-image payload did not restore as an empty scene".to_owned());
    }
    if !evidence.rollback.new_to_old_refused {
        return Err("a pre-image receiver accepted an image payload".to_owned());
    }
    if !evidence.rollback.downgraded_payload_accepted || !evidence.downgrade_exports_nothing {
        return Err("disabling images did not produce a downgrade-safe payload".to_owned());
    }
    if !evidence.current_receiver.current_accepts_image_payload
        || !evidence.current_receiver.current_accepts_pre_image_payload
    {
        return Err("a current receiver refused a payload it must accept".to_owned());
    }
    Ok(())
}

fn encode_state(sessions: Vec<HandoffSession>, version: u32) -> Result<Vec<u8>, String> {
    let state = HandoffState {
        version,
        sessions,
        workspaces: Vec::new(),
        workspace_tree: None,
        windows: Vec::new(),
    };
    rmp_serde::to_vec_named(&state).map_err(|error| format!("encode handoff state: {error}"))
}

/// The smallest `HandoffSession` the version gate needs to see.
fn session_stub(image_state: Option<SessionImageHandoff>) -> HandoffSession {
    HandoffSession {
        session_id: scribe_common::ids::SessionId::new(),
        workspace_id: scribe_common::ids::WorkspaceId::new(),
        child_pid: 1,
        child_identity: None,
        cols: 80,
        rows: 24,
        cell_width: 8,
        cell_height: 16,
        snapshot: None,
        session_replay: None,
        title: None,
        icon_title: None,
        shell_name: "shell".to_owned(),
        task_label: None,
        codex_task_label: None,
        cwd: None,
        context: None,
        ai_state: None,
        ai_provider_hint: None,
        prompt_state: None,
        image_state,
    }
}
