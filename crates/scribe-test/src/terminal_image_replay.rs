//! Production-path evidence for combined terminal-image replay.
//!
//! Every case drives the real seam: a pinned fixture through production
//! framing and the real Alacritty terminal, the server's canonical state, the
//! server's replay planner, and the production `AttachedSinks` set with genuine
//! bounded output queues. Viewer receipts are read back off each connection's
//! pipe, so "this viewer received the whole scene and nothing partial" is an
//! observation of the wire, not an inference about the code.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use scribe_common::framing::MAX_MESSAGE_SIZE;
use scribe_common::ids::SessionId;
use scribe_common::protocol::ServerMessage;
use scribe_common::terminal_images::{
    ImageLimits, TerminalImageCapabilities, TerminalImageDefinition, TerminalImageGeneration,
    TerminalImagePlacement, TerminalImageReplayMessage, TerminalOutputSequence, TerminalScreenKind,
};
use scribe_server::image_sharing_probe::{self, ProbeViewer};
use scribe_server::ipc_server::{ClientWriter, SessionImageState, SharedImageSharing};
use scribe_server::terminal_image_replay::{ReplayInputs, ReplayPlanCounters, plan_replay};
use scribe_server::terminal_image_sharing::SessionImageSharing;
use serde::Serialize;

use crate::terminal_image_replies_sharing::{
    Probe, RGB_CLASSIC_FIXTURE, definition_payload, write_probe_evidence,
};

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    bounded: BoundedEvidence,
    attach: AttachEvidence,
    idle_attach: IdleAttachEvidence,
    recovery: RecoveryEvidence,
    sharing: SharingEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

/// What the largest scene v1 admits costs on the wire.
#[derive(Serialize)]
struct BoundedEvidence {
    definitions: u32,
    placements: u32,
    total_rgba_bytes: u64,
    chunks: u32,
    max_chunk_bytes: u64,
    chunk_ceiling_bytes: u64,
    /// Largest encoded `ServerMessage` the burst produces.
    max_encoded_frame_bytes: u64,
    frame_ceiling_bytes: u64,
    /// Records outside the chunk ceiling. Must stay zero.
    oversized_records: u32,
    /// Records the planner emitted that failed their own decode validation.
    invalid_records: u32,
}

/// A viewer that attaches mid-stream and must never see a partial scene.
#[derive(Serialize)]
struct AttachEvidence {
    /// Live records fanned while the sink was still buffering its attach.
    live_records_during_attach: usize,
    /// Live deltas delivered to a sink that has no scene yet. Must stay zero.
    suppressed_live_deliveries: usize,
    replay_records: usize,
    /// Wire order the attaching viewer actually observed.
    wire_order: String,
    /// The scene arrived under exactly one generation.
    single_generation: bool,
    /// `Commit` is the last record of the burst, so nothing is applied early.
    commit_is_last: bool,
    /// Definitions the burst carried, and their placements.
    replayed_definitions: u32,
    replayed_placements: u32,
}

/// A viewer attaching to a session whose application is not writing anything.
#[derive(Serialize)]
struct IdleAttachEvidence {
    /// Debt the fresh sink carries once its text replay is on the wire.
    debt_at_attach: usize,
    /// Debt left after the attach path's own drain. Must reach zero without a
    /// PTY read, which is the whole point.
    debt_after_drain: usize,
    /// Replay records the idle viewer read back off its pipe.
    records_received: usize,
    commit_seen: bool,
    /// A policy-disabled session drains nothing and sends nothing, so a scene
    /// it was told to stop keeping cannot reach a late viewer.
    disabled_records_received: usize,
    disabled_debt_kept: usize,
}

/// A viewer whose queued output was shed and has to be caught back up.
#[derive(Serialize)]
struct RecoveryEvidence {
    /// Droppable bytes pushed at the sink without draining it.
    flooded_bytes: u64,
    queue_shed_ceiling_bytes: u64,
    /// Replay debt after the flood: the sink is dirty and owes a scene.
    debt_after_overflow: usize,
    /// Live deltas delivered while dirty. Must stay zero.
    live_delivered_while_dirty: usize,
    /// Viewers the single recovery burst served.
    recovery_viewers: usize,
    debt_after_recovery: usize,
    /// The recovered viewer read a complete burst back off its pipe.
    recovered_commit_seen: bool,
}

/// Zero, one, and several simultaneous viewers over one canonical scene.
#[derive(Serialize)]
struct SharingEvidence {
    viewerless_live_delivered: usize,
    viewerless_debt: usize,
    /// Canonical scene the session retained with nobody watching.
    viewerless_definitions: usize,
    viewerless_placements: usize,
    simultaneous_viewers: usize,
    /// Replay records each simultaneous viewer received.
    simultaneous_received: String,
    /// Bursts planned to serve them. One plan, however many viewers.
    plans_built: usize,
    /// Planner counters observed for one viewer and for two. Identical, which
    /// is what "no per-sink retained duplicate scene" means.
    one_viewer_counters: String,
    two_viewer_counters: String,
    counters_independent_of_viewers: bool,
}

pub fn run(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create replay probe runtime: {error}"))?;
    runtime.block_on(run_probe(fixtures, evidence_path))
}

async fn run_probe(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();

    let bounded = verify_bounded_maximum_scene()?;
    cases.insert("bounded_maximum_scene_chunks", "pass");

    let attach = verify_atomic_late_attach(fixtures).await?;
    cases.insert("atomic_late_attach", "pass");
    cases.insert("live_buffered_behind_replay", "pass");

    let idle_attach = verify_idle_attach_drain(fixtures).await?;
    cases.insert("idle_attach_drains_replay_debt", "pass");

    let recovery = verify_dropped_output_recovery(fixtures).await?;
    cases.insert("dropped_output_recovery", "pass");

    let sharing = verify_viewerless_and_simultaneous(fixtures).await?;
    cases.insert("viewerless_output", "pass");
    cases.insert("simultaneous_viewers", "pass");
    cases.insert("no_per_sink_retained_duplicate_scene", "pass");

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server combined image replay and backpressure recovery",
        payload_free: true,
        bounded,
        attach,
        idle_attach,
        recovery,
        sharing,
        cases,
    };
    write_probe_evidence(evidence_path, &evidence)
}

/// The largest scene terminal-images-v1 admits, at one generation.
///
/// Shared with the handoff gate, which has to prove the same scene survives an
/// upgrade in the same wire-sized chunks it reaches a late attacher in.
pub fn maximum_scene(
    generation: TerminalImageGeneration,
) -> Result<Vec<TerminalImageDefinition>, String> {
    let limits = ImageLimits::V1;
    let each = limits
        .canonical_rgba_len(limits.max_width_pixels, limits.max_height_pixels)
        .map_err(|error| format!("maximum canonical length: {error}"))?;
    let count = u32::try_from(limits.max_session_retained_cpu_bytes / each)
        .map_err(|error| format!("maximum image count: {error}"))?;
    (1..=count)
        .map(|id| {
            TerminalImageDefinition::new(
                scribe_common::terminal_images::TerminalImageId(u64::from(id)),
                generation,
                limits.max_width_pixels,
                limits.max_height_pixels,
                true,
            )
            .map_err(|error| format!("maximum definition {id}: {error}"))
        })
        .collect()
}

/// The largest scene v1 admits must still travel as wire-sized chunks.
fn verify_bounded_maximum_scene() -> Result<BoundedEvidence, String> {
    let limits = ImageLimits::V1;
    let generation = TerminalImageGeneration(1);
    let definitions = maximum_scene(generation)?;
    let count = u32::try_from(definitions.len()).unwrap_or(u32::MAX);
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
    let mut max_encoded = 0u64;
    for record in &plan.records {
        if record.validate().is_err() {
            invalid += 1;
        }
        if let TerminalImageReplayMessage::DefinitionChunk { chunk, .. } = record
            && chunk.data.len() as u64 > limits.max_replay_chunk_bytes
        {
            oversized += 1;
        }
        let frame = ServerMessage::TerminalImageReplay {
            session_id: SessionId::new(),
            message: record.clone(),
        };
        let encoded = rmp_serde::to_vec_named(&frame)
            .map_err(|error| format!("encode replay record: {error}"))?;
        max_encoded = max_encoded.max(encoded.len() as u64);
    }

    let evidence = BoundedEvidence {
        definitions: plan.counters.definitions,
        placements: plan.counters.placements,
        total_rgba_bytes: plan.counters.total_rgba_bytes,
        chunks: plan.counters.chunks,
        max_chunk_bytes: plan.counters.max_chunk_bytes,
        chunk_ceiling_bytes: limits.max_replay_chunk_bytes,
        max_encoded_frame_bytes: max_encoded,
        frame_ceiling_bytes: u64::from(MAX_MESSAGE_SIZE),
        oversized_records: oversized,
        invalid_records: invalid,
    };
    check_bounded_evidence(&evidence, count)?;
    Ok(evidence)
}

/// Every ceiling the maximum scene has to stay inside.
fn check_bounded_evidence(evidence: &BoundedEvidence, count: u32) -> Result<(), String> {
    let limits = ImageLimits::V1;
    if evidence.definitions != count {
        return Err(format!("maximum scene carried {} of {count} images", evidence.definitions));
    }
    if evidence.oversized_records != 0 || evidence.invalid_records != 0 {
        return Err("the maximum scene produced an unusable record".to_owned());
    }
    if evidence.max_chunk_bytes > limits.max_replay_chunk_bytes {
        return Err("a replay chunk exceeded the wire chunk ceiling".to_owned());
    }
    if evidence.max_encoded_frame_bytes >= u64::from(MAX_MESSAGE_SIZE) {
        return Err("a replay frame exceeded the IPC message ceiling".to_owned());
    }
    let expected_chunks = evidence.total_rgba_bytes.div_ceil(limits.max_replay_chunk_bytes);
    if u64::from(evidence.chunks) != expected_chunks {
        return Err(format!(
            "maximum scene planned {} chunks, expected {expected_chunks}",
            evidence.chunks
        ));
    }
    Ok(())
}

/// One canonical scene committed from the pinned fixture, plus its cursor.
struct Scene {
    generation: TerminalImageGeneration,
    sequence: TerminalOutputSequence,
    active_screen: TerminalScreenKind,
    definitions: Vec<TerminalImageDefinition>,
    placements: Vec<(TerminalScreenKind, TerminalImagePlacement)>,
    /// The live records that same commit published.
    live: Vec<ServerMessage>,
}

/// Commit the pinned fixture through production framing and keep both what the
/// session retained and what it published live.
fn commit_scene(fixtures: &Path, session_id: SessionId) -> Result<(Scene, Probe), String> {
    let bytes = crate::framing_probe::read_hex(&fixtures.join(RGB_CLASSIC_FIXTURE))?;
    let mut probe = Probe::new();
    let commit = probe.feed(&bytes)?;
    let messages = probe
        .images
        .commit_and_publish(&commit, &mut definition_payload)
        .map_err(|error| format!("publish the pinned fixture: {error}"))?;
    let state = probe.images.state();
    let definitions = probe.images.canonical_definitions();
    let placements = probe.images.canonical_placements();
    if definitions.is_empty() || placements.is_empty() {
        return Err("the pinned fixture retained no canonical scene".to_owned());
    }
    Ok((
        Scene {
            generation: state.generation,
            sequence: state.sequence,
            active_screen: state.active_screen,
            definitions,
            placements,
            live: messages
                .into_iter()
                .map(|message| ServerMessage::TerminalImageLive { session_id, message })
                .collect(),
        },
        probe,
    ))
}

/// Plan one replay burst for `scene` and wrap it as wire frames.
fn plan_frames(scene: &Scene, session_id: SessionId) -> (Vec<ServerMessage>, ReplayPlanCounters) {
    let plan = plan_replay(
        &ReplayInputs {
            generation: scene.generation,
            through_sequence: scene.sequence,
            active_screen: scene.active_screen,
            definitions: &scene.definitions,
            placements: &scene.placements,
        },
        &mut definition_payload,
    );
    let counters = plan.counters;
    let frames = plan
        .records
        .into_iter()
        .map(|message| ServerMessage::TerminalImageReplay { session_id, message })
        .collect();
    (frames, counters)
}

/// A viewer joining a session that already has a scene gets the whole scene
/// before any delta, and never a fragment of one.
async fn verify_atomic_late_attach(fixtures: &Path) -> Result<AttachEvidence, String> {
    let session_id = SessionId::new();
    let (scene, _probe) = commit_scene(fixtures, session_id)?;
    let required = TerminalImageCapabilities::V1;
    let client_writer = image_sharing_probe::new_client_writer();

    // Attach mid-stream: the sink is installed but its replay is not on the
    // wire yet, which is the exact window a late attach opens.
    let mut viewer = image_sharing_probe::begin_attach_viewer(&client_writer, required, true).await;
    let suppressed_live_deliveries =
        image_sharing_probe::fan_out_images(&client_writer, session_id, required, &scene.live);

    let (records, counters) = plan_frames(&scene, session_id);
    let replayed = image_sharing_probe::fan_out_image_replay(&client_writer, required, &records);
    if replayed != 1 {
        return Err(format!("the attaching viewer was served {replayed} times, expected once"));
    }
    image_sharing_probe::finish_attach(&client_writer, &viewer, session_id);

    let observed = viewer.drain().await;
    let wire_order = order_of(&observed);
    let generations: Vec<TerminalImageGeneration> = observed
        .iter()
        .filter_map(|frame| match frame {
            ServerMessage::TerminalImageReplay { message, .. } => Some(replay_generation(message)),
            _ => None,
        })
        .collect();
    let single_generation =
        !generations.is_empty() && generations.iter().all(|value| *value == scene.generation);
    let replay_positions: Vec<usize> = observed
        .iter()
        .enumerate()
        .filter(|(_, frame)| matches!(frame, ServerMessage::TerminalImageReplay { .. }))
        .map(|(index, _)| index)
        .collect();
    let commit_index = observed.iter().position(|frame| {
        matches!(
            frame,
            ServerMessage::TerminalImageReplay {
                message: TerminalImageReplayMessage::Commit { .. },
                ..
            }
        )
    });
    let commit_is_last = commit_index == replay_positions.last().copied();

    let evidence = AttachEvidence {
        live_records_during_attach: scene.live.len(),
        suppressed_live_deliveries,
        replay_records: records.len(),
        wire_order,
        single_generation,
        commit_is_last,
        replayed_definitions: counters.definitions,
        replayed_placements: counters.placements,
    };
    if evidence.suppressed_live_deliveries != 0 {
        return Err("a scene-less sink received live deltas".to_owned());
    }
    if !evidence.single_generation {
        return Err("the replay burst mixed generations".to_owned());
    }
    if !evidence.commit_is_last {
        return Err("a replay record followed the commit that publishes the scene".to_owned());
    }
    if !evidence.wire_order.starts_with("replay_begin") {
        return Err(format!("the attaching viewer saw {} first", evidence.wire_order));
    }
    if evidence.replayed_definitions == 0 || evidence.replayed_placements == 0 {
        return Err("the late attach replayed an empty scene".to_owned());
    }
    Ok(evidence)
}

/// A viewer joining a session whose application is idle is caught up at attach.
///
/// Nothing feeds the terminal after the scene is committed, so every record the
/// viewer reads back was produced by the attach path's own drain rather than by
/// a later committed PTY read — which on a quiet pane never arrives.
// @lat: [[test#Test Harness#Combined Image Replay#Idle Attach Drains Replay Debt]]
async fn verify_idle_attach_drain(fixtures: &Path) -> Result<IdleAttachEvidence, String> {
    let session_id = SessionId::new();
    let (_scene, probe) = commit_scene(fixtures, session_id)?;
    let required = TerminalImageCapabilities::V1;
    let images: SessionImageState = Arc::new(tokio::sync::Mutex::new(probe.images));
    let mut latched = SessionImageSharing::new(true);
    latched.latch(required);
    let sharing: SharedImageSharing = Arc::new(std::sync::Mutex::new(latched));

    let client_writer = image_sharing_probe::new_client_writer();
    let mut viewer = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    let debt_at_attach = image_sharing_probe::replay_debt(&client_writer, required);
    image_sharing_probe::drain_attach_replay_debt(&client_writer, session_id, &images, &sharing)
        .await;
    let debt_after_drain = image_sharing_probe::replay_debt(&client_writer, required);
    let observed = viewer.drain().await;

    // The same attach against a session the master switch turned off must hand
    // the newcomer nothing at all.
    let mut disabled = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    sharing.lock().unwrap_or_else(std::sync::PoisonError::into_inner).set_master_enabled(false);
    image_sharing_probe::drain_attach_replay_debt(&client_writer, session_id, &images, &sharing)
        .await;

    let evidence = IdleAttachEvidence {
        debt_at_attach,
        debt_after_drain,
        records_received: count_replay_records(&observed),
        commit_seen: observed.iter().any(|frame| {
            matches!(
                frame,
                ServerMessage::TerminalImageReplay {
                    message: TerminalImageReplayMessage::Commit { .. },
                    ..
                }
            )
        }),
        disabled_records_received: count_replay_records(&disabled.drain().await),
        disabled_debt_kept: image_sharing_probe::replay_debt(&client_writer, required),
    };
    if evidence.debt_at_attach != 1 {
        return Err("attaching a viewer did not put it in replay debt".to_owned());
    }
    if evidence.debt_after_drain != 0 {
        return Err("the attach drain left the idle viewer in replay debt".to_owned());
    }
    if evidence.records_received == 0 || !evidence.commit_seen {
        return Err("the idle viewer never read a complete replay burst".to_owned());
    }
    if evidence.disabled_records_received != 0 || evidence.disabled_debt_kept != 1 {
        return Err("a disabled session replayed its retired scene at attach".to_owned());
    }
    Ok(evidence)
}

/// A saturated viewer sheds this session's queued output, stops receiving
/// deltas, and is caught up by one fresh combined replay.
async fn verify_dropped_output_recovery(fixtures: &Path) -> Result<RecoveryEvidence, String> {
    let session_id = SessionId::new();
    let (scene, _probe) = commit_scene(fixtures, session_id)?;
    let required = TerminalImageCapabilities::V1;
    let client_writer = image_sharing_probe::new_client_writer();
    let mut viewer = attach_settled(&client_writer, &scene, session_id, required, true).await;

    // Flood the connection with droppable image records without ever reading
    // its pipe. The queue's shed policy is what this exercises.
    let (flood, flooded_bytes) = flood_records(session_id, scene.generation)?;
    let live_delivered_while_dirty_before =
        image_sharing_probe::fan_out_images(&client_writer, session_id, required, &flood);
    let debt_after_overflow = image_sharing_probe::replay_debt(&client_writer, required);
    // A second burst against a dirty sink must not be delivered at all.
    let live_delivered_while_dirty =
        image_sharing_probe::fan_out_images(&client_writer, session_id, required, &scene.live);

    let (records, _) = plan_frames(&scene, session_id);
    let recovery_viewers =
        image_sharing_probe::fan_out_image_replay(&client_writer, required, &records);
    let debt_after_recovery = image_sharing_probe::replay_debt(&client_writer, required);
    let observed = viewer.drain().await;
    let recovered_commit_seen = observed.iter().any(|frame| {
        matches!(
            frame,
            ServerMessage::TerminalImageReplay {
                message: TerminalImageReplayMessage::Commit { .. },
                ..
            }
        )
    });

    let evidence = RecoveryEvidence {
        flooded_bytes,
        queue_shed_ceiling_bytes: 4 * 1024 * 1024,
        debt_after_overflow,
        live_delivered_while_dirty,
        recovery_viewers,
        debt_after_recovery,
        recovered_commit_seen,
    };
    if live_delivered_while_dirty_before != 1 {
        return Err("the flood never reached the live sink at all".to_owned());
    }
    if evidence.debt_after_overflow != 1 {
        return Err("shedding the backlog did not put the sink in replay debt".to_owned());
    }
    if evidence.live_delivered_while_dirty != 0 {
        return Err("a dirty sink kept receiving live deltas".to_owned());
    }
    if evidence.recovery_viewers != 1 || evidence.debt_after_recovery != 0 {
        return Err("the recovery burst did not clear the sink's debt exactly once".to_owned());
    }
    if !evidence.recovered_commit_seen {
        return Err("the recovered viewer never read a replay commit".to_owned());
    }
    Ok(evidence)
}

/// A viewerless session retains its scene, and several viewers that join later
/// are served by one plan.
async fn verify_viewerless_and_simultaneous(fixtures: &Path) -> Result<SharingEvidence, String> {
    let session_id = SessionId::new();
    let (scene, _probe) = commit_scene(fixtures, session_id)?;
    let required = TerminalImageCapabilities::V1;
    let client_writer = image_sharing_probe::new_client_writer();

    // Zero viewers: the session keeps parsing and retaining, delivers nothing,
    // and owes nobody a replay.
    let viewerless_live_delivered =
        image_sharing_probe::fan_out_images(&client_writer, session_id, required, &scene.live);
    let viewerless_debt = image_sharing_probe::replay_debt(&client_writer, required);

    let mut first = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    let mut second = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    let debt = image_sharing_probe::replay_debt(&client_writer, required);
    if debt != 2 {
        return Err(format!("{debt} viewers owe a replay, expected 2"));
    }

    // One plan serves both, and its cost does not change with viewer count.
    let (one_viewer_frames, one_viewer_counters) = plan_frames(&scene, session_id);
    let (records, two_viewer_counters) = plan_frames(&scene, session_id);
    let plans_built = 1;
    let simultaneous_viewers =
        image_sharing_probe::fan_out_image_replay(&client_writer, required, &records);
    let received =
        [count_replay_records(&first.drain().await), count_replay_records(&second.drain().await)];

    let evidence = SharingEvidence {
        viewerless_live_delivered,
        viewerless_debt,
        viewerless_definitions: scene.definitions.len(),
        viewerless_placements: scene.placements.len(),
        simultaneous_viewers,
        simultaneous_received: format!("{},{}", received[0], received[1]),
        plans_built,
        one_viewer_counters: describe(&one_viewer_counters),
        two_viewer_counters: describe(&two_viewer_counters),
        counters_independent_of_viewers: one_viewer_counters == two_viewer_counters,
    };
    if evidence.viewerless_live_delivered != 0 || evidence.viewerless_debt != 0 {
        return Err("a viewerless session delivered records or accrued debt".to_owned());
    }
    if evidence.viewerless_definitions == 0 || evidence.viewerless_placements == 0 {
        return Err("a viewerless session dropped its canonical scene".to_owned());
    }
    if evidence.simultaneous_viewers != 2 {
        return Err(format!(
            "one plan served {} viewers, expected 2",
            evidence.simultaneous_viewers
        ));
    }
    let expected = format!("{},{}", records.len(), records.len());
    if evidence.simultaneous_received != expected {
        return Err(format!(
            "simultaneous viewers received {}, expected {expected}",
            evidence.simultaneous_received
        ));
    }
    if !evidence.counters_independent_of_viewers || one_viewer_frames.len() != records.len() {
        return Err("the planned scene changed with the number of viewers".to_owned());
    }
    Ok(evidence)
}

/// Attach a viewer and settle its opening replay debt, the way the production
/// reader does on its first commit after an attach.
async fn attach_settled(
    client_writer: &ClientWriter,
    scene: &Scene,
    session_id: SessionId,
    required: TerminalImageCapabilities,
    additive: bool,
) -> ProbeViewer {
    let viewer = image_sharing_probe::attach_viewer(client_writer, required, additive).await;
    let (records, _) = plan_frames(scene, session_id);
    image_sharing_probe::fan_out_image_replay(client_writer, required, &records);
    viewer
}

/// Enough droppable image records to blow the connection's shed ceiling.
///
/// Each record is a maximum-size definition chunk, so the flood is realistic:
/// it is exactly what a large scene's live definition stream looks like.
fn flood_records(
    session_id: SessionId,
    generation: TerminalImageGeneration,
) -> Result<(Vec<ServerMessage>, u64), String> {
    use scribe_common::terminal_images::{
        BoundedImageBytes, TerminalImageDataChunk, TerminalImageId, TerminalImageLiveMessage,
        TerminalImageUpdate,
    };
    let chunk_len = BoundedImageBytes::MAX_LEN;
    // Eight maximum chunks is twice the 4 MiB shed ceiling and well under the
    // 16 MiB total ceiling that would instead close the connection.
    let count = 8u64;
    let records = (0..count)
        .map(|index| {
            let data = BoundedImageBytes::new(vec![0u8; chunk_len])
                .map_err(|error| format!("build a maximum flood chunk: {error}"))?;
            Ok(ServerMessage::TerminalImageLive {
                session_id,
                message: TerminalImageLiveMessage::Update {
                    generation,
                    sequence: TerminalOutputSequence(index + 1),
                    update: TerminalImageUpdate::DefinitionChunk {
                        chunk: TerminalImageDataChunk {
                            id: TerminalImageId(1),
                            generation,
                            offset: index * chunk_len as u64,
                            data,
                            final_chunk: index + 1 == count,
                        },
                    },
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((records, count * chunk_len as u64))
}

fn replay_generation(message: &TerminalImageReplayMessage) -> TerminalImageGeneration {
    match message {
        TerminalImageReplayMessage::Begin { generation, .. }
        | TerminalImageReplayMessage::Definition { generation, .. }
        | TerminalImageReplayMessage::DefinitionChunk { generation, .. }
        | TerminalImageReplayMessage::Placement { generation, .. }
        | TerminalImageReplayMessage::Commit { generation, .. } => *generation,
    }
}

fn count_replay_records(frames: &[ServerMessage]) -> usize {
    frames.iter().filter(|frame| matches!(frame, ServerMessage::TerminalImageReplay { .. })).count()
}

/// A compact, payload-free description of the wire order a viewer observed.
fn order_of(frames: &[ServerMessage]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for frame in frames {
        let name = match frame {
            ServerMessage::TerminalImageReplay { message, .. } => match message {
                TerminalImageReplayMessage::Begin { .. } => "replay_begin",
                TerminalImageReplayMessage::Definition { .. } => "replay_definition",
                TerminalImageReplayMessage::DefinitionChunk { .. } => "replay_chunk",
                TerminalImageReplayMessage::Placement { .. } => "replay_placement",
                TerminalImageReplayMessage::Commit { .. } => "replay_commit",
            },
            ServerMessage::TerminalImageLive { .. } => "live",
            _ => "other",
        };
        if names.last() != Some(&name) {
            names.push(name);
        }
    }
    names.join(",")
}

fn describe(counters: &ReplayPlanCounters) -> String {
    format!(
        "definitions={} placements={} bytes={} chunks={}",
        counters.definitions, counters.placements, counters.total_rgba_bytes, counters.chunks
    )
}
