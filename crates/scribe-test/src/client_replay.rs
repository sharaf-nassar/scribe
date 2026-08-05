//! Production-path evidence for staged client terminal-image replay.
//!
//! Every record this gate applies was planned by the server's own replay
//! planner from canonical state the production seam built out of real PTY
//! bytes, and every record is applied through the production client scene.
//! "The client never showed a partial snapshot" is therefore an observation of
//! the published `Arc` identity, not an inference about the code.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Weak};

use scribe_client::terminal_image_scene::{
    CommittedImageScene, LiveImageScene, LiveSceneApply, LiveSceneError, MAX_BUFFERED_LIVE_RECORDS,
};
use scribe_common::terminal_images::{
    TerminalImageDefinition, TerminalImageGeneration, TerminalImageLiveMessage,
    TerminalImagePlacement, TerminalImageReplayMessage, TerminalOutputSequence, TerminalScreenKind,
};
use scribe_server::terminal_image_replay::{ReplayInputs, plan_replay};
use serde::Serialize;

use crate::terminal_image_convergence::{concat, control, transmit_display};
use crate::terminal_image_replies_sharing::{Probe, definition_payload, write_probe_evidence};

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    staging: StagingEvidence,
    ordering: OrderingEvidence,
    staleness: StalenessEvidence,
    recovery: RecoveryEvidence,
    cleanup: CleanupEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

/// A whole snapshot applied one record at a time.
#[derive(Serialize)]
struct StagingEvidence {
    /// Records the burst carried, including its begin and commit.
    replay_records: usize,
    /// Records accepted without publishing anything.
    staged_records: usize,
    /// Times the published scene's identity changed. Exactly one: the commit.
    published_identity_changes: usize,
    /// Staged records after which the published scene held any image state.
    /// Must stay zero — that is what "no partial scene" means.
    partial_observations: usize,
    committed_definitions: usize,
    committed_placements: usize,
    committed_rgba_bytes: u64,
    /// The same counts read off the server's canonical state.
    canonical_definitions: usize,
    canonical_placements: usize,
    canonical_rgba_bytes: u64,
}

/// Live records that arrive while a snapshot stages.
#[derive(Serialize)]
struct OrderingEvidence {
    /// Live records handed to the client between begin and commit.
    buffered_live_records: usize,
    /// Live records applied to the published scene before the commit. Must
    /// stay zero.
    applied_before_commit: usize,
    /// Scene the staged path published after draining its buffer.
    staged_scene: String,
    /// Scene the same snapshot plus the same live records produce when no
    /// buffering happens at all.
    direct_scene: String,
    /// Buffering the live stream is order-preserving, not merely lossless.
    matches_direct_live_order: bool,
    /// The drained live records also advanced the published output cursor.
    through_sequence_advanced: bool,
}

/// Records that describe a scene the client has already moved past.
#[derive(Serialize)]
struct StalenessEvidence {
    /// Typed refusal of a whole snapshot from an older generation.
    stale_snapshot_rejected: &'static str,
    /// The refusal left the published scene untouched.
    published_scene_preserved: bool,
    /// Older-generation live records buffered behind a newer snapshot.
    stale_buffered_records: usize,
    /// Definitions the drained buffer resurrected. Must stay zero.
    resurrected_definitions: usize,
    resurrected_placements: usize,
    definitions_after_drain: usize,
}

/// Corrupt bursts and the one clean burst that recovers from them.
#[derive(Serialize)]
struct RecoveryEvidence {
    /// Each corruption and the typed error it produced.
    corruptions: BTreeMap<&'static str, String>,
    /// Every corruption left the previously published scene in place.
    scene_preserved_across_failures: &'static str,
    /// Every corruption also dropped its staged snapshot and buffer.
    staging_cleared_after_failure: &'static str,
    /// One well-formed burst afterwards published the new scene.
    fresh_replay_recovered: &'static str,
    recovered_definitions: usize,
    recovered_placements: usize,
}

/// What an abandoned or superseded scene leaves behind.
#[derive(Serialize)]
struct CleanupEvidence {
    /// Canonical pixels of a superseded scene, after the replacing commit.
    superseded_pixels_released: bool,
    /// Retained bytes after a snapshot that follows an abandoned one. Equal to
    /// the new snapshot alone, so nothing accumulated across the failure.
    retained_rgba_bytes_after_abandon: u64,
    expected_rgba_bytes: u64,
    /// Live records the buffer accepts behind a staging snapshot.
    live_buffer_ceiling: usize,
    buffer_overflow_error: String,
    /// Overflow abandoned the snapshot instead of growing without bound.
    buffer_overflow_aborted_snapshot: bool,
    buffered_after_overflow: usize,
}

/// One canonical server scene, ready to be planned into a replay burst.
struct ServerScene {
    generation: TerminalImageGeneration,
    sequence: TerminalOutputSequence,
    active_screen: TerminalScreenKind,
    definitions: Vec<TerminalImageDefinition>,
    placements: Vec<(TerminalScreenKind, TerminalImagePlacement)>,
}

impl ServerScene {
    fn rgba_bytes(&self) -> u64 {
        self.definitions.iter().map(|definition| definition.rgba_bytes).sum()
    }

    /// Plan this scene through the production replay planner.
    fn plan(&self) -> Vec<TerminalImageReplayMessage> {
        plan_replay(
            &ReplayInputs {
                generation: self.generation,
                through_sequence: self.sequence,
                active_screen: self.active_screen,
                definitions: &self.definitions,
                placements: &self.placements,
            },
            &mut definition_payload,
        )
        .records
    }
}

/// The canonical snapshots and live bursts every case draws from.
struct Stages {
    /// One image, defined and placed.
    first: ServerScene,
    /// The live burst that created it.
    first_live: Vec<TerminalImageLiveMessage>,
    /// A second image beside the first.
    second_live: Vec<TerminalImageLiveMessage>,
    /// A third image beside both.
    third_live: Vec<TerminalImageLiveMessage>,
    /// All three images.
    third: ServerScene,
    /// One image under a new generation opened by a hard reset.
    reset: ServerScene,
}

// @lat: [[test#Test Harness#Staged Client Image Replay#Production Staging Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let stages = build_stages()?;

    let staging = case_no_partial_scene(&stages)?;
    let ordering = case_ordered_post_commit_live(&stages)?;
    let staleness = case_stale_generation(&stages)?;
    let recovery = case_corrupt_replay_recovery(&stages)?;
    let cleanup = case_cleanup(&stages)?;

    let cases = BTreeMap::from([
        ("no_partial_scene", "pass"),
        ("ordered_post_commit_live", "pass"),
        ("stale_generation_never_resurrects", "pass"),
        ("corrupt_replay_recovers", "pass"),
        ("staged_state_cleaned_up", "pass"),
    ]);
    write_probe_evidence(
        evidence_path,
        &Evidence {
            schema_version: 1,
            status: "pass",
            engine: "scribe-client staged terminal image replay",
            payload_free: true,
            staging,
            ordering,
            staleness,
            recovery,
            cleanup,
            cases,
        },
    )
}

/// Drive real PTY bytes through the production seam and keep both the
/// canonical snapshots and the live bursts published between them.
fn build_stages() -> Result<Stages, String> {
    let mut probe = Probe::new();
    let first_live = commit(&mut probe, &concat(&[control("\x1b[3;5H"), transmit_display(7)]))?;
    let first = snapshot(&probe);
    let second_live = commit(&mut probe, &concat(&[control("\x1b[5;2H"), transmit_display(8)]))?;
    let third_live = commit(&mut probe, &concat(&[control("\x1b[7;1H"), transmit_display(9)]))?;
    let third = snapshot(&probe);

    // A hard reset opens the next generation, which is the only thing that
    // makes an older snapshot genuinely stale rather than merely behind.
    commit(&mut probe, &control("\x1bc"))?;
    commit(&mut probe, &concat(&[control("\x1b[2;2H"), transmit_display(11)]))?;
    let reset = snapshot(&probe);
    if reset.generation <= first.generation {
        return Err("the hard reset did not open a newer generation".to_owned());
    }
    if first.definitions.len() != 1 || third.definitions.len() != 3 {
        return Err(format!(
            "the seam retained {} then {} definitions, expected 1 then 3",
            first.definitions.len(),
            third.definitions.len()
        ));
    }
    Ok(Stages { first, first_live, second_live, third_live, third, reset })
}

/// Commit one PTY read through the production seam and return what it
/// published live.
fn commit(probe: &mut Probe, bytes: &[u8]) -> Result<Vec<TerminalImageLiveMessage>, String> {
    let commit = probe.feed(bytes)?;
    probe
        .images
        .commit_and_publish(&commit, &mut definition_payload)
        .map_err(|error| format!("publish the production burst: {error}"))
}

fn snapshot(probe: &Probe) -> ServerScene {
    let state = probe.images.state();
    ServerScene {
        generation: state.generation,
        sequence: state.sequence,
        active_screen: state.active_screen,
        definitions: probe.images.canonical_definitions(),
        placements: probe.images.canonical_placements(),
    }
}

/// Payload-free description of one published scene, used to compare two
/// application orders without embedding any pixels.
fn scene_core(scene: &CommittedImageScene) -> String {
    let definitions: Vec<(u64, u64)> = scene
        .definitions
        .iter()
        .map(|definition| (definition.metadata.id.0, definition.metadata.rgba_bytes))
        .collect();
    format!(
        "generation={:?} through={:?} definitions={definitions:?} primary={:?} alternate={:?} \
         screen={:?} retained={}",
        scene.generation.map(|value| value.0),
        scene.through_sequence.map(|value| value.0),
        placement_keys(&scene.primary_placements),
        placement_keys(&scene.alternate_placements),
        scene.active_screen,
        scene.retained_rgba_bytes,
    )
}

fn placement_keys(placements: &[TerminalImagePlacement]) -> Vec<(u64, u64)> {
    placements.iter().map(|placement| (placement.image_id.0, placement.id.0)).collect()
}

/// Apply a whole planned burst and return the scene its commit published.
fn apply_burst(
    scene: &mut LiveImageScene,
    records: &[TerminalImageReplayMessage],
) -> Result<Arc<CommittedImageScene>, String> {
    let mut published = None;
    for record in records {
        if let LiveSceneApply::Committed(committed) = scene
            .apply_replay(record.clone())
            .map_err(|error| format!("apply replay record: {error}"))?
        {
            published = Some(committed);
        }
    }
    published.ok_or_else(|| "the burst published nothing".to_owned())
}

/// Every record before the commit stages off-screen; the commit publishes the
/// whole snapshot at once.
fn case_no_partial_scene(stages: &Stages) -> Result<StagingEvidence, String> {
    let records = stages.third.plan();
    let mut scene = LiveImageScene::default();
    let mut staged_records = 0usize;
    let mut published_identity_changes = 0usize;
    let mut partial_observations = 0usize;

    for record in &records {
        let before = scene.committed();
        let outcome = scene
            .apply_replay(record.clone())
            .map_err(|error| format!("staging the planned burst: {error}"))?;
        let after = scene.committed();
        if !Arc::ptr_eq(&before, &after) {
            published_identity_changes += 1;
        }
        if matches!(outcome, LiveSceneApply::Staged) {
            staged_records += 1;
            if !after.definitions.is_empty()
                || !after.primary_placements.is_empty()
                || !after.alternate_placements.is_empty()
            {
                partial_observations += 1;
            }
        }
    }

    let committed = scene.committed();
    let evidence = StagingEvidence {
        replay_records: records.len(),
        staged_records,
        published_identity_changes,
        partial_observations,
        committed_definitions: committed.definitions.len(),
        committed_placements: committed.placements().len(),
        committed_rgba_bytes: committed.retained_rgba_bytes,
        canonical_definitions: stages.third.definitions.len(),
        canonical_placements: stages.third.placements.len(),
        canonical_rgba_bytes: stages.third.rgba_bytes(),
    };
    if evidence.published_identity_changes != 1 {
        return Err(format!(
            "the burst republished {} times, expected once",
            evidence.published_identity_changes
        ));
    }
    if evidence.partial_observations != 0 {
        return Err("a staged record leaked into the published scene".to_owned());
    }
    if evidence.staged_records + 1 != evidence.replay_records {
        return Err("a record other than the commit published a scene".to_owned());
    }
    if evidence.committed_definitions != evidence.canonical_definitions
        || evidence.committed_placements != evidence.canonical_placements
        || evidence.committed_rgba_bytes != evidence.canonical_rgba_bytes
    {
        return Err("the published scene is not the canonical one".to_owned());
    }
    Ok(evidence)
}

/// Live records that arrive mid-snapshot are held back and then applied in
/// arrival order, producing exactly the scene an unbuffered stream would.
fn case_ordered_post_commit_live(stages: &Stages) -> Result<OrderingEvidence, String> {
    let records = stages.first.plan();
    let (body, commit) =
        records.split_at(records.len().checked_sub(1).ok_or("empty planned burst")?);
    let later: Vec<TerminalImageLiveMessage> =
        stages.second_live.iter().chain(&stages.third_live).cloned().collect();

    let mut buffering = LiveImageScene::default();
    for record in body {
        buffering
            .apply_replay(record.clone())
            .map_err(|error| format!("stage the snapshot: {error}"))?;
    }
    let before_buffering = buffering.committed();
    let mut applied_before_commit = 0usize;
    for message in &later {
        buffering
            .apply(message.clone())
            .map_err(|error| format!("buffer a live record: {error}"))?;
        if !Arc::ptr_eq(&before_buffering, &buffering.committed()) {
            applied_before_commit += 1;
        }
    }
    let buffered_live_records = buffering.buffered_live_len();
    for record in commit {
        buffering
            .apply_replay(record.clone())
            .map_err(|error| format!("commit the snapshot: {error}"))?;
    }
    let staged_scene = buffering.committed();

    // The same snapshot and the same live records, with nothing ever buffered.
    let mut direct = LiveImageScene::default();
    apply_burst(&mut direct, &records)?;
    for message in &later {
        direct
            .apply(message.clone())
            .map_err(|error| format!("apply a live record directly: {error}"))?;
    }
    let direct_scene = direct.committed();

    let evidence = OrderingEvidence {
        buffered_live_records,
        applied_before_commit,
        staged_scene: scene_core(&staged_scene),
        direct_scene: scene_core(&direct_scene),
        matches_direct_live_order: scene_core(&staged_scene) == scene_core(&direct_scene),
        through_sequence_advanced: staged_scene.through_sequence > Some(stages.first.sequence),
    };
    if evidence.applied_before_commit != 0 {
        return Err("a buffered live record changed the published scene".to_owned());
    }
    if evidence.buffered_live_records != later.len() {
        return Err(format!(
            "the client buffered {} of {} live records",
            evidence.buffered_live_records,
            later.len()
        ));
    }
    if !evidence.matches_direct_live_order {
        return Err(format!(
            "the drained order diverged: staged {} direct {}",
            evidence.staged_scene, evidence.direct_scene
        ));
    }
    if !evidence.through_sequence_advanced {
        return Err("the drained live records did not advance the output cursor".to_owned());
    }
    Ok(evidence)
}

/// An older generation cannot come back, whether it arrives as a whole
/// snapshot or as live records buffered behind a newer one.
fn case_stale_generation(stages: &Stages) -> Result<StalenessEvidence, String> {
    let reset_records = stages.reset.plan();
    let mut scene = LiveImageScene::default();
    let published = apply_burst(&mut scene, &reset_records)?;

    let stale_records = stages.first.plan();
    let first = stale_records.first().ok_or("empty stale burst")?;
    let stale_snapshot_rejected = match scene.apply_replay(first.clone()) {
        Err(LiveSceneError::StaleGeneration) => "stale_generation",
        Err(error) => return Err(format!("unexpected stale-snapshot error: {error}")),
        Ok(_) => return Err("an older-generation snapshot was accepted".to_owned()),
    };
    let published_scene_preserved = Arc::ptr_eq(&published, &scene.committed());

    // The same older generation, this time as live records buffered behind the
    // newer snapshot. The drain must drop them rather than replay them.
    let stale_live: Vec<TerminalImageLiveMessage> =
        stages.first_live.iter().chain(&stages.second_live).cloned().collect();
    let mut drained = LiveImageScene::default();
    let (body, commit) =
        reset_records.split_at(reset_records.len().checked_sub(1).ok_or("empty reset burst")?);
    for record in body {
        drained
            .apply_replay(record.clone())
            .map_err(|error| format!("stage the newer snapshot: {error}"))?;
    }
    for message in &stale_live {
        drained
            .apply(message.clone())
            .map_err(|error| format!("buffer a stale live record: {error}"))?;
    }
    let stale_buffered_records = drained.buffered_live_len();
    for record in commit {
        drained
            .apply_replay(record.clone())
            .map_err(|error| format!("commit the newer snapshot: {error}"))?;
    }
    let after = drained.committed();
    let snapshot_ids: Vec<u64> =
        stages.reset.definitions.iter().map(|definition| definition.id.0).collect();
    let resurrected_definitions = after
        .definitions
        .iter()
        .filter(|definition| !snapshot_ids.contains(&definition.metadata.id.0))
        .count();
    let resurrected_placements = after
        .placements()
        .iter()
        .filter(|placement| !snapshot_ids.contains(&placement.image_id.0))
        .count();

    let evidence = StalenessEvidence {
        stale_snapshot_rejected,
        published_scene_preserved,
        stale_buffered_records,
        resurrected_definitions,
        resurrected_placements,
        definitions_after_drain: after.definitions.len(),
    };
    if !evidence.published_scene_preserved {
        return Err("the refused snapshot still replaced the published scene".to_owned());
    }
    if evidence.stale_buffered_records != stale_live.len() {
        return Err("the client did not buffer the stale live stream".to_owned());
    }
    if evidence.resurrected_definitions != 0 || evidence.resurrected_placements != 0 {
        return Err("the drain resurrected superseded image state".to_owned());
    }
    if evidence.definitions_after_drain != stages.reset.definitions.len() {
        return Err("the drain changed the snapshot's own definitions".to_owned());
    }
    Ok(evidence)
}

/// Every way a burst can be corrupt leaves the previous scene published, and
/// one clean burst afterwards recovers.
fn case_corrupt_replay_recovery(stages: &Stages) -> Result<RecoveryEvidence, String> {
    let target = stages.third.plan();
    let mut scene = LiveImageScene::default();
    let published = apply_burst(&mut scene, &stages.first.plan())?;

    let mut corruptions = BTreeMap::new();
    let mut scene_preserved = true;
    let mut staging_cleared = true;
    for (name, records) in build_corruptions(&target, &stages.reset.plan())? {
        let failure = records
            .into_iter()
            .find_map(|record| scene.apply_replay(record).err())
            .ok_or_else(|| format!("corruption {name} was accepted"))?;
        corruptions.insert(name, failure.to_string());
        scene_preserved &= Arc::ptr_eq(&published, &scene.committed());
        staging_cleared &= !scene.is_staging_replay() && scene.buffered_live_len() == 0;
    }

    let recovered = apply_burst(&mut scene, &target)?;
    let recovered_definitions = recovered.definitions.len();
    let evidence = RecoveryEvidence {
        corruptions,
        scene_preserved_across_failures: verdict(scene_preserved),
        staging_cleared_after_failure: verdict(staging_cleared),
        fresh_replay_recovered: verdict(!Arc::ptr_eq(&published, &recovered)),
        recovered_definitions,
        recovered_placements: recovered.placements().len(),
    };
    if !scene_preserved {
        return Err("a corrupt burst changed the published scene".to_owned());
    }
    if !staging_cleared {
        return Err("a corrupt burst left staged state behind".to_owned());
    }
    if Arc::ptr_eq(&published, &recovered)
        || recovered_definitions != stages.third.definitions.len()
    {
        return Err("the recovery burst did not publish the current scene".to_owned());
    }
    Ok(evidence)
}

const fn verdict(passed: bool) -> &'static str {
    if passed { "pass" } else { "fail" }
}

/// One named corruption and the records that produce it.
type CorruptBurst = (&'static str, Vec<TerminalImageReplayMessage>);

/// Every way a planned burst can be corrupt, built by permuting records the
/// planner really emitted rather than by inventing record shapes.
fn build_corruptions(
    target: &[TerminalImageReplayMessage],
    foreign: &[TerminalImageReplayMessage],
) -> Result<Vec<CorruptBurst>, String> {
    let find = |records: &[TerminalImageReplayMessage],
                want: fn(&TerminalImageReplayMessage) -> bool| {
        records.iter().find(|record| want(record)).cloned()
    };
    let begin = target.first().cloned().ok_or("empty target burst")?;
    let commit = target.last().cloned().ok_or("empty target burst")?;
    let definition =
        find(target, |record| matches!(record, TerminalImageReplayMessage::Definition { .. }))
            .ok_or("no planned definition")?;
    let chunk =
        find(target, |record| matches!(record, TerminalImageReplayMessage::DefinitionChunk { .. }))
            .ok_or("no planned chunk")?;
    let placement =
        find(target, |record| matches!(record, TerminalImageReplayMessage::Placement { .. }))
            .ok_or("no planned placement")?;
    let foreign_definition =
        find(foreign, |record| matches!(record, TerminalImageReplayMessage::Definition { .. }))
            .ok_or("the foreign burst carried no definition")?;

    Ok(vec![
        ("record_without_begin", vec![definition.clone()]),
        ("commit_without_begin", vec![commit.clone()]),
        ("placement_before_definition", vec![begin.clone(), placement]),
        ("dropped_definition_chunk", vec![begin.clone(), definition.clone(), commit.clone()]),
        ("truncated_burst", vec![begin.clone(), definition, chunk, commit]),
        ("foreign_generation_record", vec![begin, foreign_definition]),
    ])
}

/// An abandoned snapshot and a superseded scene both release what they held.
fn case_cleanup(stages: &Stages) -> Result<CleanupEvidence, String> {
    let first = stages.first.plan();
    let reset = stages.reset.plan();
    let third = stages.third.plan();

    let mut scene = LiveImageScene::default();
    let pixels: Weak<[u8]> = {
        let committed = apply_burst(&mut scene, &first)?;
        let definition =
            committed.definitions.first().ok_or("the first snapshot carried no definition")?;
        Arc::downgrade(&definition.rgba)
    };
    apply_burst(&mut scene, &reset)?;
    let superseded_pixels_released = pixels.upgrade().is_none();

    // Abandon a snapshot part-way, then commit a whole one: nothing the
    // abandoned burst staged may still be charged to the pane.
    let mut abandoned = LiveImageScene::default();
    apply_burst(&mut abandoned, &first)?;
    for record in third.iter().take(third.len().saturating_sub(1)) {
        abandoned
            .apply_replay(record.clone())
            .map_err(|error| format!("stage the abandoned burst: {error}"))?;
    }
    abandoned.discard_partial();
    let recommitted = apply_burst(&mut abandoned, &reset)?;

    // A live stream that outgrows its buffer abandons the snapshot rather than
    // the bound.
    let mut flooded = LiveImageScene::default();
    for record in reset.iter().take(reset.len().saturating_sub(1)) {
        flooded
            .apply_replay(record.clone())
            .map_err(|error| format!("stage the flooded burst: {error}"))?;
    }
    let mut overflow = None;
    for index in 0..=MAX_BUFFERED_LIVE_RECORDS {
        let sequence = TerminalOutputSequence(u64::try_from(index).unwrap_or(u64::MAX));
        let message = TerminalImageLiveMessage::Begin {
            generation: stages.reset.generation,
            sequence: TerminalOutputSequence(
                stages.reset.sequence.0.saturating_add(sequence.0).saturating_add(1),
            ),
        };
        if let Err(error) = flooded.apply(message) {
            overflow = Some(error);
            break;
        }
    }
    let buffer_overflow_error = match overflow {
        Some(LiveSceneError::LiveBufferOverflow) => "live_buffer_overflow".to_owned(),
        Some(error) => return Err(format!("unexpected buffering error: {error}")),
        None => return Err("the live buffer accepted more than its ceiling".to_owned()),
    };

    let evidence = CleanupEvidence {
        superseded_pixels_released,
        retained_rgba_bytes_after_abandon: recommitted.retained_rgba_bytes,
        expected_rgba_bytes: stages.reset.rgba_bytes(),
        live_buffer_ceiling: MAX_BUFFERED_LIVE_RECORDS,
        buffer_overflow_error,
        buffer_overflow_aborted_snapshot: !flooded.is_staging_replay(),
        buffered_after_overflow: flooded.buffered_live_len(),
    };
    if !evidence.superseded_pixels_released {
        return Err("a superseded scene still owns its canonical pixels".to_owned());
    }
    if evidence.retained_rgba_bytes_after_abandon != evidence.expected_rgba_bytes {
        return Err("an abandoned snapshot stayed charged to the pane".to_owned());
    }
    if !evidence.buffer_overflow_aborted_snapshot || evidence.buffered_after_overflow != 0 {
        return Err("buffer overflow left staged state behind".to_owned());
    }
    Ok(evidence)
}
