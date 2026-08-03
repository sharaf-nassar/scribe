//! Fixture driver for the production GPUI client's CPU image scene.

use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

use scribe_client::terminal_image_scene::{
    CommittedImageScene, LiveImageScene, LiveSceneApply, LiveSceneError,
    capability_mismatch_message, filter_terminal_image_placeholders,
};
use scribe_common::terminal_images::{
    TerminalImageCapabilityMismatch, TerminalImageDefinition, TerminalImageGeneration,
    TerminalImageId, TerminalImageLiveMessage, TerminalImageUpdate, TerminalOutputSequence,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct SceneFixture {
    live_messages: Vec<TerminalImageLiveMessage>,
    copy_input: String,
    copy_expected: String,
    mismatch: TerminalImageCapabilityMismatch,
    mismatch_contains: String,
    quota_definition_count: u32,
}

type AppliedMessages = (Vec<Arc<CommittedImageScene>>, bool, bool);

pub fn verify(fixtures: &Path, output: &Path) -> Result<(), String> {
    let fixture: SceneFixture = serde_json::from_slice(
        &fs::read(fixtures).map_err(|error| format!("read {}: {error}", fixtures.display()))?,
    )
    .map_err(|error| format!("decode {}: {error}", fixtures.display()))?;

    let (commits, atomic_staging, stale_rejected) = apply_messages(fixture.live_messages)?;

    let initial = commits.first().ok_or_else(|| "missing initial scene commit".to_owned())?;
    let ordered_initial_placements =
        initial.placements().iter().map(|placement| placement.id.0).collect::<Vec<_>>() == [10, 20];

    let replaced = commits.get(1).ok_or_else(|| "missing replacement scene commit".to_owned())?;
    let replacement_placement = replaced.placements().first();
    let replacement_and_grid_effects = replaced.definitions.len() == 2
        && replaced
            .definitions
            .iter()
            .find(|definition| definition.metadata.id.0 == 1)
            .is_some_and(|definition| definition.rgba.as_ref() == [9, 8, 7, 6])
        && replaced.placements().len() == 1
        && replacement_placement
            .is_some_and(|placement| placement.id.0 == 11 && placement.anchor.row == 2)
        && replaced.last_grid_effects.len() == 2;

    let deleted = commits.get(2).ok_or_else(|| "missing deletion scene commit".to_owned())?;
    let deletion_frees_definition = deleted.placements().is_empty()
        && deleted.definitions.len() == 1
        && deleted.definitions.first().is_some_and(|definition| definition.metadata.id.0 == 2);

    let reset = commits.get(3).ok_or_else(|| "missing reset scene commit".to_owned())?;
    let reset_cleanup = reset.definitions.is_empty()
        && reset.primary_placements.is_empty()
        && reset.alternate_placements.is_empty()
        && reset.retained_rgba_bytes == 0;

    let after_partial =
        commits.get(4).ok_or_else(|| "missing partial cleanup commit".to_owned())?;
    let partial_and_stale_cleanup = stale_rejected
        && after_partial.definitions.is_empty()
        && after_partial.placements().is_empty();

    let typed_quota_error = verify_quota_error(fixture.quota_definition_count)?;
    let placeholder_copy_filtering =
        filter_terminal_image_placeholders(&fixture.copy_input) == fixture.copy_expected;
    let mismatch_update_required =
        capability_mismatch_message(fixture.mismatch).contains(&fixture.mismatch_contains);

    let evidence = BTreeMap::from([
        ("atomic_staging", atomic_staging),
        ("ordered_initial_placements", ordered_initial_placements),
        ("replacement_and_grid_effects", replacement_and_grid_effects),
        ("deletion_frees_definition", deletion_frees_definition),
        ("reset_cleanup", reset_cleanup),
        ("partial_and_stale_cleanup", partial_and_stale_cleanup),
        ("placeholder_copy_filtering", placeholder_copy_filtering),
        ("typed_quota_error", typed_quota_error),
        ("mismatch_update_required", mismatch_update_required),
    ]);
    write_evidence(output, &evidence)?;

    if evidence.values().all(|passed| *passed) {
        Ok(())
    } else {
        Err("one or more client scene fixture assertions failed".to_owned())
    }
}

fn apply_messages(messages: Vec<TerminalImageLiveMessage>) -> Result<AppliedMessages, String> {
    let mut scene = LiveImageScene::default();
    let mut commits = Vec::new();
    let mut atomic_staging = true;
    let mut stale_rejected = false;
    for message in messages {
        let before = scene.committed();
        match scene.apply(message) {
            Ok(LiveSceneApply::Staged) => {
                atomic_staging &= Arc::ptr_eq(&before, &scene.committed());
            }
            Ok(LiveSceneApply::Committed(committed)) => commits.push(committed),
            Err(LiveSceneError::StaleGeneration | LiveSceneError::StaleSequence) => {
                stale_rejected = true;
            }
            Err(error) => return Err(format!("unexpected live scene error: {error}")),
        }
    }
    Ok((commits, atomic_staging, stale_rejected))
}

fn write_evidence(output: &Path, evidence: &BTreeMap<&str, bool>) -> Result<(), String> {
    let encoded = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode client scene evidence: {error}"))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    fs::write(output, encoded).map_err(|error| format!("write {}: {error}", output.display()))
}

fn verify_quota_error(definition_count: u32) -> Result<bool, String> {
    let generation = TerminalImageGeneration(1);
    let sequence = TerminalOutputSequence(1);
    let mut scene = LiveImageScene::default();
    scene
        .apply(TerminalImageLiveMessage::Begin { generation, sequence })
        .map_err(|error| error.to_string())?;
    for id in 0..definition_count {
        let definition = TerminalImageDefinition::new(
            TerminalImageId(u64::from(id).saturating_add(1)),
            generation,
            1,
            1,
            true,
        )
        .map_err(|error| error.to_string())?;
        let result = scene.apply(TerminalImageLiveMessage::Update {
            generation,
            sequence,
            update: TerminalImageUpdate::Define { definition },
        });
        if matches!(result, Err(LiveSceneError::LimitExceeded(_))) {
            return Ok(true);
        }
        result.map_err(|error| error.to_string())?;
    }
    Ok(false)
}
