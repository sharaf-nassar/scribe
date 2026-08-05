//! Immutable, bounded CPU state for one pane's live terminal-image scene.
//!
//! The server sends a live change as `Begin`, zero or more ordered updates,
//! and `Commit`. This module applies the burst to an unpublished clone and
//! swaps one `Arc` only after every definition is complete and every quota has
//! been checked. A malformed, stale, interrupted, or partial burst therefore
//! cannot leak into paint state.
//!
//! A replay burst is assembled the same way, except that it builds an empty
//! scene from scratch rather than cloning the published one: it is a whole
//! snapshot, not a delta. Live records that arrive while a snapshot is being
//! staged are buffered and applied in arrival order after the swap, so the
//! published scene never mixes half a snapshot with a later delta.

use std::{collections::HashSet, sync::Arc};

use scribe_common::terminal_images::{
    BoundedImageBytes, ImageBoundError, ImageLimitName, ImageLimits, PixelRect, TerminalGridEffect,
    TerminalImageCapabilityMismatch, TerminalImageCellClip, TerminalImageDataChunk,
    TerminalImageDefinition, TerminalImageDelete, TerminalImageDeleteScope,
    TerminalImageGeneration, TerminalImageId, TerminalImageLiveMessage, TerminalImagePlacement,
    TerminalImagePlacementKind, TerminalImageRejection, TerminalImageReplayMessage,
    TerminalImageUpdate, TerminalOutputSequence, TerminalPlacementId, TerminalScreenKind,
};
use unicode_width::UnicodeWidthChar;

/// Kitty's private-use image placeholder cell.
pub const KITTY_IMAGE_PLACEHOLDER: char = '\u{10eeee}';

/// A completed canonical RGBA definition owned by an immutable scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveImageDefinition {
    pub metadata: TerminalImageDefinition,
    pub rgba: Arc<[u8]>,
}

/// Immutable render input published after one valid live commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedImageScene {
    pub generation: Option<TerminalImageGeneration>,
    pub through_sequence: Option<TerminalOutputSequence>,
    pub definitions: Vec<LiveImageDefinition>,
    pub primary_placements: Vec<TerminalImagePlacement>,
    pub alternate_placements: Vec<TerminalImagePlacement>,
    pub active_screen: TerminalScreenKind,
    pub retained_rgba_bytes: u64,
    pub last_grid_effects: Vec<TerminalGridEffect>,
    pub last_rejection: Option<TerminalImageRejection>,
}

impl Default for CommittedImageScene {
    fn default() -> Self {
        Self {
            generation: None,
            through_sequence: None,
            definitions: Vec::new(),
            primary_placements: Vec::new(),
            alternate_placements: Vec::new(),
            active_screen: TerminalScreenKind::Primary,
            retained_rgba_bytes: 0,
            last_grid_effects: Vec::new(),
            last_rejection: None,
        }
    }
}

impl CommittedImageScene {
    /// Placements belonging to the terminal's active screen, in operation order.
    #[must_use]
    pub fn placements(&self) -> &[TerminalImagePlacement] {
        match self.active_screen {
            TerminalScreenKind::Primary => &self.primary_placements,
            TerminalScreenKind::Alternate => &self.alternate_placements,
        }
    }

    /// Apply one terminal grid mutation to this committed scene.
    ///
    /// Live bursts use the same operation before publication. Exposing the
    /// operation on owned scenes also keeps renderer-boundary probes on the
    /// production margin and source-cropping path.
    pub fn apply_grid_effect(&mut self, effect: &TerminalGridEffect) {
        apply_grid_effect(self, effect);
    }

    /// Apply one protocol-normalized deletion to this owned scene.
    ///
    /// `screen` restricts the deletion to one grid; `None` keeps the legacy
    /// scope rules, where identity deletes reach both screens.
    pub fn apply_delete(
        &mut self,
        delete: TerminalImageDelete,
        screen: Option<TerminalScreenKind>,
    ) {
        apply_delete(self, delete, screen);
    }

    fn placements_mut(&mut self) -> &mut Vec<TerminalImagePlacement> {
        let screen = self.active_screen;
        self.screen_placements_mut(screen)
    }

    fn screen_placements_mut(
        &mut self,
        screen: TerminalScreenKind,
    ) -> &mut Vec<TerminalImagePlacement> {
        match screen {
            TerminalScreenKind::Primary => &mut self.primary_placements,
            TerminalScreenKind::Alternate => &mut self.alternate_placements,
        }
    }

    fn screen_placements(&self, screen: TerminalScreenKind) -> &[TerminalImagePlacement] {
        match screen {
            TerminalScreenKind::Primary => &self.primary_placements,
            TerminalScreenKind::Alternate => &self.alternate_placements,
        }
    }

    fn all_placements_len(&self) -> usize {
        self.primary_placements.len().saturating_add(self.alternate_placements.len())
    }

    fn definition(&self, id: TerminalImageId) -> Option<&LiveImageDefinition> {
        self.definitions.iter().find(|definition| definition.metadata.id == id)
    }
}

/// Result of consuming one live record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSceneApply {
    /// A begin or update was accepted but remains unpublished.
    Staged,
    /// A complete burst atomically replaced the published scene.
    Committed(Arc<CommittedImageScene>),
}

/// Typed rejection for an invalid or out-of-order client-side live burst.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveSceneError {
    #[error("terminal image live update arrived without begin")]
    UpdateWithoutBegin,
    #[error("terminal image live commit arrived without begin")]
    CommitWithoutBegin,
    #[error("terminal image live record does not match the open burst")]
    BoundaryMismatch,
    #[error("terminal image generation is stale")]
    StaleGeneration,
    #[error("terminal image output sequence is stale")]
    StaleSequence,
    #[error("terminal image definition chunks are incomplete")]
    IncompleteDefinition,
    #[error("terminal image definition was not started")]
    DefinitionNotStarted,
    #[error("terminal image definition chunk offset is not contiguous")]
    NonContiguousChunk,
    #[error("terminal image placement references a missing definition")]
    MissingDefinition,
    #[error("terminal image placement geometry is invalid")]
    InvalidPlacement,
    #[error("terminal image allocation failed")]
    AllocationFailed,
    #[error("terminal image limit exceeded: {0:?}")]
    LimitExceeded(ImageLimitName),
    #[error("terminal image replay record arrived without begin")]
    ReplayRecordWithoutBegin,
    #[error("terminal image replay does not carry the scene it declared")]
    ReplayCountMismatch,
    #[error("terminal image live records buffered behind a replay outgrew their bound")]
    LiveBufferOverflow,
    #[error(transparent)]
    Bound(#[from] ImageBoundError),
}

/// Live records held while a replay stages.
///
/// The server suppresses live deltas to a sink that owes a replay, so this
/// only ever absorbs the boundary between the two streams. Overflow abandons
/// the staged snapshot instead of growing without bound or applying part of a
/// stream the client can no longer order.
pub const MAX_BUFFERED_LIVE_RECORDS: usize = 4_096;

struct PendingDefinition {
    metadata: TerminalImageDefinition,
    rgba: Vec<u8>,
}

/// One unpublished scene under construction, plus the definitions still
/// accumulating chunks. Live bursts and replay snapshots share it, so both
/// paths run the same quota, contiguity, and placement checks.
struct SceneDraft {
    generation: TerminalImageGeneration,
    scene: CommittedImageScene,
    definitions: Vec<PendingDefinition>,
}

struct PendingBurst {
    sequence: TerminalOutputSequence,
    draft: SceneDraft,
}

/// What a replay's `Begin` promises the rest of its burst will carry.
#[derive(Clone, Copy)]
struct ReplayDeclaration {
    definitions: u32,
    placements: u32,
    rgba_bytes: u64,
    active_screen: Option<TerminalScreenKind>,
}

/// An off-screen snapshot being assembled from a generation-tagged replay.
struct StagingReplay {
    declared_definitions: u32,
    declared_placements: u32,
    declared_rgba_bytes: u64,
    definitions: u32,
    placements: u32,
    rgba_bytes: u64,
    draft: SceneDraft,
}

/// Live-operation state machine for one pane.
pub struct LiveImageScene {
    committed: Arc<CommittedImageScene>,
    pending: Option<PendingBurst>,
    replay: Option<StagingReplay>,
    /// Live records that arrived while `replay` was staging, in arrival order.
    buffered: Vec<TerminalImageLiveMessage>,
    buffered_bytes: u64,
}

impl Default for LiveImageScene {
    fn default() -> Self {
        Self {
            committed: Arc::new(CommittedImageScene::default()),
            pending: None,
            replay: None,
            buffered: Vec::new(),
            buffered_bytes: 0,
        }
    }
}

impl LiveImageScene {
    /// Current immutable scene. Its identity changes only on a valid commit.
    #[must_use]
    pub fn committed(&self) -> Arc<CommittedImageScene> {
        Arc::clone(&self.committed)
    }

    /// Discard every unpublished record — an interrupted live generation, a
    /// half-staged replay snapshot, and anything buffered behind it — without
    /// changing the published scene.
    pub fn discard_partial(&mut self) {
        self.pending = None;
        self.discard_replay();
    }

    /// Whether a replay snapshot is currently staged off-screen.
    #[must_use]
    pub fn is_staging_replay(&self) -> bool {
        self.replay.is_some()
    }

    /// Live records held behind the staged snapshot, awaiting its commit.
    #[must_use]
    pub fn buffered_live_len(&self) -> usize {
        self.buffered.len()
    }

    /// Consume one generation/sequence-tagged live record.
    ///
    /// While a replay stages, the record is buffered instead of applied: the
    /// snapshot it belongs behind has not been published yet, so applying it
    /// now would be a delta on a scene the client is about to replace.
    pub fn apply(
        &mut self,
        message: TerminalImageLiveMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        if self.replay.is_some() {
            return self.buffer_live(message);
        }
        self.apply_live(message)
    }

    /// Consume one generation-tagged replay record.
    ///
    /// Records stage into an empty off-screen scene; only `Commit` swaps the
    /// published `Arc`, and it also drains whatever live records arrived in the
    /// meantime. Any failure abandons the whole snapshot, leaving the published
    /// scene exactly as it was, so a corrupt burst costs a resync rather than a
    /// wrong picture.
    pub fn apply_replay(
        &mut self,
        message: TerminalImageReplayMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        let result = self.apply_replay_record(message);
        if result.is_err() {
            self.discard_replay();
        }
        result
    }

    fn buffer_live(
        &mut self,
        message: TerminalImageLiveMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        let bytes = live_payload_bytes(&message);
        let projected = self.buffered_bytes.saturating_add(bytes);
        if self.buffered.len() >= MAX_BUFFERED_LIVE_RECORDS
            || projected > ImageLimits::V1.max_session_retained_cpu_bytes
        {
            self.discard_replay();
            return Err(LiveSceneError::LiveBufferOverflow);
        }
        self.buffered_bytes = projected;
        self.buffered.push(message);
        Ok(LiveSceneApply::Staged)
    }

    fn apply_live(
        &mut self,
        message: TerminalImageLiveMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        match message {
            TerminalImageLiveMessage::Begin { generation, sequence } => {
                // A replacement begin is also the cleanup boundary for an
                // interrupted definition stream.
                self.pending = None;
                if let Some(stale) = self.stale_boundary(generation, sequence) {
                    return Err(stale);
                }
                let mut scene = (*self.committed).clone();
                scene.generation = Some(generation);
                scene.through_sequence = Some(sequence);
                scene.last_grid_effects.clear();
                scene.last_rejection = None;
                self.pending = Some(PendingBurst {
                    sequence,
                    draft: SceneDraft { generation, scene, definitions: Vec::new() },
                });
                Ok(LiveSceneApply::Staged)
            }
            TerminalImageLiveMessage::Update { generation, sequence, update } => {
                let result = self.apply_tagged_update(generation, sequence, update);
                if result.is_err() {
                    self.pending = None;
                }
                result.map(|()| LiveSceneApply::Staged)
            }
            TerminalImageLiveMessage::Commit { generation, sequence } => {
                self.commit(generation, sequence)
            }
        }
    }

    /// Why a live boundary describes a scene the published one already passed.
    fn stale_boundary(
        &self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    ) -> Option<LiveSceneError> {
        if self.committed.generation.is_some_and(|committed| generation < committed) {
            return Some(LiveSceneError::StaleGeneration);
        }
        if self.committed.through_sequence.is_some_and(|committed| sequence <= committed) {
            return Some(LiveSceneError::StaleSequence);
        }
        None
    }

    fn apply_tagged_update(
        &mut self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
        update: TerminalImageUpdate,
    ) -> Result<(), LiveSceneError> {
        let pending = self.pending.as_mut().ok_or(LiveSceneError::UpdateWithoutBegin)?;
        if pending.draft.generation != generation || pending.sequence != sequence {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        apply_update(&mut pending.draft, update)
    }

    fn commit(
        &mut self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        let Some(pending) = self.pending.take() else {
            return Err(LiveSceneError::CommitWithoutBegin);
        };
        if pending.draft.generation != generation || pending.sequence != sequence {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        if !pending.draft.definitions.is_empty() {
            return Err(LiveSceneError::IncompleteDefinition);
        }
        let committed = Arc::new(pending.draft.scene);
        self.committed = Arc::clone(&committed);
        Ok(LiveSceneApply::Committed(committed))
    }

    /// Drop the staged snapshot and everything buffered behind it. The staged
    /// definitions are the only owner of their pixels, so this is also where
    /// an abandoned snapshot's memory goes.
    fn discard_replay(&mut self) {
        self.replay = None;
        self.buffered = Vec::new();
        self.buffered_bytes = 0;
    }

    fn apply_replay_record(
        &mut self,
        message: TerminalImageReplayMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        message.validate()?;
        match message {
            TerminalImageReplayMessage::Begin {
                generation,
                definition_count,
                placement_count,
                total_rgba_bytes,
                active_screen,
                ..
            } => {
                self.begin_replay(
                    generation,
                    ReplayDeclaration {
                        definitions: definition_count,
                        placements: placement_count,
                        rgba_bytes: total_rgba_bytes,
                        active_screen,
                    },
                )?;
                Ok(LiveSceneApply::Staged)
            }
            TerminalImageReplayMessage::Definition { generation, definition } => {
                let staging = self.staging(generation)?;
                staging.definitions = staging.definitions.saturating_add(1);
                if staging.definitions > staging.declared_definitions {
                    return Err(LiveSceneError::ReplayCountMismatch);
                }
                begin_definition(&mut staging.draft, definition)?;
                Ok(LiveSceneApply::Staged)
            }
            TerminalImageReplayMessage::DefinitionChunk { generation, chunk } => {
                let staging = self.staging(generation)?;
                staging.rgba_bytes = staging.rgba_bytes.saturating_add(chunk.data.len() as u64);
                if staging.rgba_bytes > staging.declared_rgba_bytes {
                    return Err(LiveSceneError::ReplayCountMismatch);
                }
                append_definition_chunk(&mut staging.draft, &chunk)?;
                Ok(LiveSceneApply::Staged)
            }
            TerminalImageReplayMessage::Placement { generation, placement, screen } => {
                let staging = self.staging(generation)?;
                staging.placements = staging.placements.saturating_add(1);
                if staging.placements > staging.declared_placements {
                    return Err(LiveSceneError::ReplayCountMismatch);
                }
                place(&mut staging.draft, placement, screen)?;
                Ok(LiveSceneApply::Staged)
            }
            TerminalImageReplayMessage::Commit { generation, through_sequence } => {
                self.commit_replay(generation, through_sequence)
            }
        }
    }

    fn begin_replay(
        &mut self,
        generation: TerminalImageGeneration,
        declared: ReplayDeclaration,
    ) -> Result<(), LiveSceneError> {
        // Only the generation decides staleness here. A snapshot's cursor can
        // legitimately equal the published one — a reattach with no new output
        // between the two — and refusing that would strand the viewer.
        if self.committed.generation.is_some_and(|committed| generation < committed) {
            return Err(LiveSceneError::StaleGeneration);
        }
        // A snapshot supersedes any interrupted live burst and any earlier
        // snapshot attempt; neither may contribute pixels to this one.
        self.pending = None;
        self.replay = None;
        let scene = CommittedImageScene {
            active_screen: declared.active_screen.unwrap_or(TerminalScreenKind::Primary),
            ..CommittedImageScene::default()
        };
        self.replay = Some(StagingReplay {
            declared_definitions: declared.definitions,
            declared_placements: declared.placements,
            declared_rgba_bytes: declared.rgba_bytes,
            definitions: 0,
            placements: 0,
            rgba_bytes: 0,
            draft: SceneDraft { generation, scene, definitions: Vec::new() },
        });
        Ok(())
    }

    fn staging(
        &mut self,
        generation: TerminalImageGeneration,
    ) -> Result<&mut StagingReplay, LiveSceneError> {
        let staging = self.replay.as_mut().ok_or(LiveSceneError::ReplayRecordWithoutBegin)?;
        if staging.draft.generation != generation {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        Ok(staging)
    }

    fn commit_replay(
        &mut self,
        generation: TerminalImageGeneration,
        through_sequence: TerminalOutputSequence,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        let staging = self.replay.take().ok_or(LiveSceneError::ReplayRecordWithoutBegin)?;
        if staging.draft.generation != generation {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        if !staging.draft.definitions.is_empty() {
            return Err(LiveSceneError::IncompleteDefinition);
        }
        if staging.definitions != staging.declared_definitions
            || staging.placements != staging.declared_placements
            || staging.rgba_bytes != staging.declared_rgba_bytes
        {
            return Err(LiveSceneError::ReplayCountMismatch);
        }
        let mut published = staging.draft.scene;
        published.generation = Some(generation);
        published.through_sequence = Some(through_sequence);
        self.committed = Arc::new(published);
        let drained_effects = self.drain_buffered_live();
        if !drained_effects.is_empty() {
            // Each drained burst published its own effects and the next one
            // cleared them, so the caller would otherwise only ever see the
            // last burst's grid mutations.
            let mut merged = (*self.committed).clone();
            merged.last_grid_effects = drained_effects;
            self.committed = Arc::new(merged);
        }
        Ok(LiveSceneApply::Committed(Arc::clone(&self.committed)))
    }

    /// Apply the live records held behind the snapshot, in arrival order, and
    /// return every grid effect they committed.
    ///
    /// A record the snapshot already reflects is dropped rather than applied:
    /// replaying it would resurrect definitions and placements the snapshot's
    /// generation deliberately replaced.
    fn drain_buffered_live(&mut self) -> Vec<TerminalGridEffect> {
        self.buffered_bytes = 0;
        let mut effects = Vec::new();
        for message in std::mem::take(&mut self.buffered) {
            let (generation, sequence) = live_boundary(&message);
            if self.stale_boundary(generation, sequence).is_some() {
                continue;
            }
            match self.apply_live(message) {
                Ok(LiveSceneApply::Committed(scene)) => {
                    effects.extend(scene.last_grid_effects.iter().cloned());
                }
                Ok(LiveSceneApply::Staged) => {}
                Err(_) => self.pending = None,
            }
        }
        effects
    }
}

/// The generation/sequence boundary every live record carries.
fn live_boundary(
    message: &TerminalImageLiveMessage,
) -> (TerminalImageGeneration, TerminalOutputSequence) {
    match *message {
        TerminalImageLiveMessage::Begin { generation, sequence }
        | TerminalImageLiveMessage::Update { generation, sequence, .. }
        | TerminalImageLiveMessage::Commit { generation, sequence } => (generation, sequence),
    }
}

/// Canonical pixels one buffered live record would retain.
fn live_payload_bytes(message: &TerminalImageLiveMessage) -> u64 {
    match message {
        TerminalImageLiveMessage::Update {
            update: TerminalImageUpdate::DefinitionChunk { chunk },
            ..
        } => chunk.data.len() as u64,
        _ => 0,
    }
}

fn apply_update(draft: &mut SceneDraft, update: TerminalImageUpdate) -> Result<(), LiveSceneError> {
    match update {
        TerminalImageUpdate::Define { definition } => begin_definition(draft, definition),
        TerminalImageUpdate::DefinitionChunk { chunk } => append_definition_chunk(draft, &chunk),
        TerminalImageUpdate::Place { placement, screen } => place(draft, placement, screen),
        TerminalImageUpdate::Delete { delete, screen } => {
            // Kitty specifies that every delete aborts all incomplete uploads.
            draft.definitions.clear();
            draft.scene.apply_delete(delete, screen);
            Ok(())
        }
        TerminalImageUpdate::GridEffect { effect } => {
            draft.scene.apply_grid_effect(&effect);
            draft.scene.last_grid_effects.push(effect);
            Ok(())
        }
        TerminalImageUpdate::Rejected { rejection } => {
            draft.scene.last_rejection = Some(rejection);
            Ok(())
        }
    }
}

fn begin_definition(
    draft: &mut SceneDraft,
    definition: TerminalImageDefinition,
) -> Result<(), LiveSceneError> {
    if definition.generation != draft.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    definition.validate()?;
    draft.definitions.retain(|item| item.metadata.id != definition.id);

    let replacing = draft.scene.definition(definition.id).is_some();
    let projected_count = draft
        .scene
        .definitions
        .len()
        .saturating_add(draft.definitions.len())
        .saturating_add(usize::from(!replacing));
    if projected_count > ImageLimits::V1.max_images_per_session as usize {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::ImagesPerSession));
    }

    let existing_bytes =
        draft.scene.definition(definition.id).map_or(0, |item| item.metadata.rgba_bytes);
    let pending_bytes = draft
        .definitions
        .iter()
        .try_fold(0u64, |total, item| total.checked_add(item.metadata.rgba_bytes))
        .ok_or(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes))?;
    let projected = draft
        .scene
        .retained_rgba_bytes
        .saturating_sub(existing_bytes)
        .checked_add(pending_bytes)
        .and_then(|total| total.checked_add(definition.rgba_bytes))
        .ok_or(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes))?;
    if projected > ImageLimits::V1.max_session_retained_cpu_bytes {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes));
    }

    draft.definitions.push(PendingDefinition { metadata: definition, rgba: Vec::new() });
    Ok(())
}

fn append_definition_chunk(
    draft: &mut SceneDraft,
    chunk: &TerminalImageDataChunk,
) -> Result<(), LiveSceneError> {
    if chunk.generation != draft.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    let Some(index) =
        draft.definitions.iter().position(|definition| definition.metadata.id == chunk.id)
    else {
        return Err(LiveSceneError::DefinitionNotStarted);
    };
    let definition =
        draft.definitions.get_mut(index).ok_or(LiveSceneError::DefinitionNotStarted)?;
    chunk.validate(&definition.metadata)?;
    if chunk.offset != definition.rgba.len() as u64 {
        return Err(LiveSceneError::NonContiguousChunk);
    }
    definition.rgba.try_reserve(chunk.data.len()).map_err(|_| LiveSceneError::AllocationFailed)?;
    definition.rgba.extend_from_slice(chunk.data.as_slice());
    if !chunk.final_chunk {
        return Ok(());
    }
    if definition.rgba.len() as u64 != definition.metadata.rgba_bytes {
        return Err(LiveSceneError::IncompleteDefinition);
    }

    let complete = draft.definitions.remove(index);
    install_definition(&mut draft.scene, complete);
    Ok(())
}

fn install_definition(scene: &mut CommittedImageScene, definition: PendingDefinition) {
    let image_id = definition.metadata.id;
    scene.definitions.retain(|item| item.metadata.id != image_id);
    scene.primary_placements.retain(|placement| placement.image_id != image_id);
    scene.alternate_placements.retain(|placement| placement.image_id != image_id);
    scene.definitions.push(LiveImageDefinition {
        metadata: definition.metadata,
        rgba: Arc::from(definition.rgba),
    });
    scene.retained_rgba_bytes = scene.definitions.iter().map(|item| item.metadata.rgba_bytes).sum();
}

fn place(
    draft: &mut SceneDraft,
    placement: TerminalImagePlacement,
    screen: Option<TerminalScreenKind>,
) -> Result<(), LiveSceneError> {
    if placement.generation != draft.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    let definition =
        draft.scene.definition(placement.image_id).ok_or(LiveSceneError::MissingDefinition)?;
    validate_placement(&placement, &definition.metadata)?;

    let screen = screen.unwrap_or(draft.scene.active_screen);
    let key = placement_key(&placement);
    let replacing =
        draft.scene.screen_placements(screen).iter().any(|existing| placement_key(existing) == key);
    if draft.scene.all_placements_len().saturating_add(usize::from(!replacing))
        > ImageLimits::V1.max_placements_per_session as usize
    {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::PlacementsPerSession));
    }
    let placements = draft.scene.screen_placements_mut(screen);
    placements.retain(|existing| placement_key(existing) != key);
    placements.push(placement);
    Ok(())
}

fn placement_key(placement: &TerminalImagePlacement) -> (TerminalImageId, TerminalPlacementId) {
    (placement.image_id, placement.id)
}

fn validate_placement(
    placement: &TerminalImagePlacement,
    definition: &TerminalImageDefinition,
) -> Result<(), LiveSceneError> {
    placement.validate_scalars()?;
    let PixelRect { x, y, width, height } = placement.source;
    let source_right = x.checked_add(width).ok_or(LiveSceneError::InvalidPlacement)?;
    let source_bottom = y.checked_add(height).ok_or(LiveSceneError::InvalidPlacement)?;
    if source_right > definition.width || source_bottom > definition.height {
        return Err(LiveSceneError::InvalidPlacement);
    }
    Ok(())
}

fn apply_delete(
    scene: &mut CommittedImageScene,
    delete: TerminalImageDelete,
    screen: Option<TerminalScreenKind>,
) {
    let applies = |placement: &TerminalImagePlacement| placement.matches_delete(&delete);
    // An explicit screen always wins. Otherwise Kitty's identity scopes reach
    // both grids and every geometric scope stays on the active one.
    let both_screens = screen.is_none()
        && matches!(
            delete.scope,
            TerminalImageDeleteScope::Image | TerminalImageDeleteScope::Placement
        );
    let swept = screen.unwrap_or(scene.active_screen);
    let mut selected_images = HashSet::new();
    if delete.free_image_data {
        if both_screens {
            selected_images.extend(
                scene
                    .primary_placements
                    .iter()
                    .chain(&scene.alternate_placements)
                    .filter(|placement| applies(placement))
                    .map(|placement| placement.image_id),
            );
        } else {
            selected_images.extend(
                scene
                    .screen_placements(swept)
                    .iter()
                    .filter(|placement| applies(placement))
                    .map(|placement| placement.image_id),
            );
        }
        if delete.scope == TerminalImageDeleteScope::Image {
            selected_images.extend(delete.image_id);
        }
    }
    if both_screens {
        scene.primary_placements.retain(|placement| !applies(placement));
        scene.alternate_placements.retain(|placement| !applies(placement));
    } else {
        scene.screen_placements_mut(swept).retain(|placement| !applies(placement));
    }
    if delete.free_image_data {
        let placed_images = scene
            .primary_placements
            .iter()
            .chain(&scene.alternate_placements)
            .map(|placement| placement.image_id)
            .collect::<HashSet<_>>();
        scene.definitions.retain(|definition| {
            !selected_images.contains(&definition.metadata.id)
                || placed_images.contains(&definition.metadata.id)
        });
        scene.retained_rgba_bytes =
            scene.definitions.iter().map(|item| item.metadata.rgba_bytes).sum();
    }
}

fn apply_grid_effect(scene: &mut CommittedImageScene, effect: &TerminalGridEffect) {
    match *effect {
        TerminalGridEffect::MoveCursor { .. } | TerminalGridEffect::SoftReset => {}
        TerminalGridEffect::Scroll { top, bottom, rows } => {
            apply_scroll(scene, top, bottom, rows);
        }
        TerminalGridEffect::EraseCells { top, left, bottom, right } => {
            let erase = TerminalImageCellClip {
                top: i32::from(top),
                left: i32::from(left),
                bottom: i32::from(bottom),
                right: i32::from(right),
            };
            scene.placements_mut().retain(|placement| {
                placement.kind != TerminalImagePlacementKind::Sixel
                    || !placement.intersects_cells(erase)
            });
        }
        TerminalGridEffect::ResizeClip { columns, rows } => {
            apply_resize_clip(scene, columns, rows);
        }
        TerminalGridEffect::SwitchScreen { screen } => {
            scene.active_screen = screen;
        }
        TerminalGridEffect::HardReset => {
            scene.definitions.clear();
            scene.primary_placements.clear();
            scene.alternate_placements.clear();
            scene.retained_rgba_bytes = 0;
            scene.active_screen = TerminalScreenKind::Primary;
            scene.last_rejection = None;
        }
    }
}

fn apply_scroll(scene: &mut CommittedImageScene, top: u16, bottom: u16, rows: i32) {
    let top = i32::from(top);
    let bottom = i32::from(bottom);
    let margin = TerminalImageCellClip {
        top,
        left: 0,
        bottom,
        right: TerminalImageCellClip::MAX_EXCLUSIVE_CELL,
    };
    scene.placements_mut().retain_mut(|placement| placement.apply_scroll(margin, rows));
}

fn apply_resize_clip(scene: &mut CommittedImageScene, columns: u16, rows: u16) {
    let viewport = TerminalImageCellClip {
        top: 0,
        left: 0,
        bottom: i32::from(rows),
        right: i32::from(columns),
    };
    scene.primary_placements.retain_mut(|placement| placement.clip_to_viewport(viewport));
    scene.alternate_placements.retain_mut(|placement| placement.clip_to_viewport(viewport));
}

/// Remove Kitty placeholder cells and only their attached coordinate marks.
///
/// Official row/column/MSB marks are zero-width and at most three follow one
/// placeholder. Restricting removal to that context preserves ordinary
/// combining text elsewhere in the selection or search query.
#[must_use]
pub fn filter_terminal_image_placeholders(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut placeholder_marks_left = 0u8;
    for character in text.chars() {
        if character == KITTY_IMAGE_PLACEHOLDER {
            placeholder_marks_left = 3;
            continue;
        }
        if placeholder_marks_left > 0 && UnicodeWidthChar::width(character) == Some(0) {
            placeholder_marks_left -= 1;
            continue;
        }
        placeholder_marks_left = 0;
        output.push(character);
    }
    output
}

/// Truthful local attach-refusal copy for the pane status strip.
#[must_use]
pub fn capability_mismatch_message(mismatch: TerminalImageCapabilityMismatch) -> String {
    format!(
        "Update Scribe to display terminal images (required: {:?}; offered: {:?})",
        mismatch.required, mismatch.offered
    )
}

/// Convenience for fixture builders that still construct bounded chunks.
pub fn bounded_chunk(bytes: Vec<u8>) -> Result<BoundedImageBytes, ImageBoundError> {
    BoundedImageBytes::new(bytes)
}
