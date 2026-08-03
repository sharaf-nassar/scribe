//! Immutable, bounded CPU state for one pane's live terminal-image scene.
//!
//! The server sends a live change as `Begin`, zero or more ordered updates,
//! and `Commit`. This module applies the burst to an unpublished clone and
//! swaps one `Arc` only after every definition is complete and every quota has
//! been checked. A malformed, stale, interrupted, or partial burst therefore
//! cannot leak into paint state.

use std::sync::Arc;

use scribe_common::terminal_images::{
    BoundedImageBytes, CellExtent, ImageBoundError, ImageLimitName, ImageLimits, PixelRect,
    TerminalGridEffect, TerminalImageCapabilityMismatch, TerminalImageDataChunk,
    TerminalImageDefinition, TerminalImageDelete, TerminalImageDeleteScope,
    TerminalImageGeneration, TerminalImageId, TerminalImageLiveMessage, TerminalImagePlacement,
    TerminalImagePlacementKind, TerminalImageProtocol, TerminalImageRejection, TerminalImageUpdate,
    TerminalOutputSequence, TerminalPlacementId, TerminalScreenKind,
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

    fn placements_mut(&mut self) -> &mut Vec<TerminalImagePlacement> {
        match self.active_screen {
            TerminalScreenKind::Primary => &mut self.primary_placements,
            TerminalScreenKind::Alternate => &mut self.alternate_placements,
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
    #[error(transparent)]
    Bound(#[from] ImageBoundError),
}

struct PendingDefinition {
    metadata: TerminalImageDefinition,
    rgba: Vec<u8>,
}

struct PendingBurst {
    generation: TerminalImageGeneration,
    sequence: TerminalOutputSequence,
    scene: CommittedImageScene,
    definitions: Vec<PendingDefinition>,
}

/// Live-operation state machine for one pane.
pub struct LiveImageScene {
    committed: Arc<CommittedImageScene>,
    pending: Option<PendingBurst>,
}

impl Default for LiveImageScene {
    fn default() -> Self {
        Self { committed: Arc::new(CommittedImageScene::default()), pending: None }
    }
}

impl LiveImageScene {
    /// Current immutable scene. Its identity changes only on a valid commit.
    #[must_use]
    pub fn committed(&self) -> Arc<CommittedImageScene> {
        Arc::clone(&self.committed)
    }

    /// Discard an interrupted generation without changing the published scene.
    pub fn discard_partial(&mut self) {
        self.pending = None;
    }

    /// Consume one generation/sequence-tagged live record.
    pub fn apply(
        &mut self,
        message: TerminalImageLiveMessage,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        match message {
            TerminalImageLiveMessage::Begin { generation, sequence } => {
                // A replacement begin is also the cleanup boundary for an
                // interrupted definition stream.
                self.pending = None;
                self.validate_begin(generation, sequence)?;
                let mut scene = (*self.committed).clone();
                scene.generation = Some(generation);
                scene.through_sequence = Some(sequence);
                scene.last_grid_effects.clear();
                scene.last_rejection = None;
                self.pending =
                    Some(PendingBurst { generation, sequence, scene, definitions: Vec::new() });
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

    fn validate_begin(
        &self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    ) -> Result<(), LiveSceneError> {
        if self.committed.generation.is_some_and(|committed| generation < committed) {
            return Err(LiveSceneError::StaleGeneration);
        }
        if self.committed.through_sequence.is_some_and(|committed| sequence <= committed) {
            return Err(LiveSceneError::StaleSequence);
        }
        Ok(())
    }

    fn apply_tagged_update(
        &mut self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
        update: TerminalImageUpdate,
    ) -> Result<(), LiveSceneError> {
        let pending = self.pending.as_mut().ok_or(LiveSceneError::UpdateWithoutBegin)?;
        if pending.generation != generation || pending.sequence != sequence {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        apply_update(pending, update)
    }

    fn commit(
        &mut self,
        generation: TerminalImageGeneration,
        sequence: TerminalOutputSequence,
    ) -> Result<LiveSceneApply, LiveSceneError> {
        let Some(pending) = self.pending.take() else {
            return Err(LiveSceneError::CommitWithoutBegin);
        };
        if pending.generation != generation || pending.sequence != sequence {
            return Err(LiveSceneError::BoundaryMismatch);
        }
        if !pending.definitions.is_empty() {
            return Err(LiveSceneError::IncompleteDefinition);
        }
        let committed = Arc::new(pending.scene);
        self.committed = Arc::clone(&committed);
        Ok(LiveSceneApply::Committed(committed))
    }
}

fn apply_update(
    pending: &mut PendingBurst,
    update: TerminalImageUpdate,
) -> Result<(), LiveSceneError> {
    match update {
        TerminalImageUpdate::Define { definition } => begin_definition(pending, definition),
        TerminalImageUpdate::DefinitionChunk { chunk } => append_definition_chunk(pending, &chunk),
        TerminalImageUpdate::Place { placement } => place(pending, placement),
        TerminalImageUpdate::Delete { delete } => {
            // Kitty specifies that every delete aborts all incomplete uploads.
            pending.definitions.clear();
            apply_delete(&mut pending.scene, delete);
            Ok(())
        }
        TerminalImageUpdate::GridEffect { effect } => {
            apply_grid_effect(&mut pending.scene, &effect);
            pending.scene.last_grid_effects.push(effect);
            Ok(())
        }
        TerminalImageUpdate::Rejected { rejection } => {
            pending.scene.last_rejection = Some(rejection);
            Ok(())
        }
    }
}

fn begin_definition(
    pending: &mut PendingBurst,
    definition: TerminalImageDefinition,
) -> Result<(), LiveSceneError> {
    if definition.generation != pending.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    definition.validate()?;
    pending.definitions.retain(|item| item.metadata.id != definition.id);

    let replacing = pending.scene.definition(definition.id).is_some();
    let projected_count = pending
        .scene
        .definitions
        .len()
        .saturating_add(pending.definitions.len())
        .saturating_add(usize::from(!replacing));
    if projected_count > ImageLimits::V1.max_images_per_session as usize {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::ImagesPerSession));
    }

    let existing_bytes =
        pending.scene.definition(definition.id).map_or(0, |item| item.metadata.rgba_bytes);
    let pending_bytes = pending
        .definitions
        .iter()
        .try_fold(0u64, |total, item| total.checked_add(item.metadata.rgba_bytes))
        .ok_or(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes))?;
    let projected = pending
        .scene
        .retained_rgba_bytes
        .saturating_sub(existing_bytes)
        .checked_add(pending_bytes)
        .and_then(|total| total.checked_add(definition.rgba_bytes))
        .ok_or(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes))?;
    if projected > ImageLimits::V1.max_session_retained_cpu_bytes {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::SessionRetainedCpuBytes));
    }

    pending.definitions.push(PendingDefinition { metadata: definition, rgba: Vec::new() });
    Ok(())
}

fn append_definition_chunk(
    pending: &mut PendingBurst,
    chunk: &TerminalImageDataChunk,
) -> Result<(), LiveSceneError> {
    if chunk.generation != pending.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    let Some(index) =
        pending.definitions.iter().position(|definition| definition.metadata.id == chunk.id)
    else {
        return Err(LiveSceneError::DefinitionNotStarted);
    };
    let definition =
        pending.definitions.get_mut(index).ok_or(LiveSceneError::DefinitionNotStarted)?;
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

    let complete = pending.definitions.remove(index);
    install_definition(&mut pending.scene, complete);
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
    pending: &mut PendingBurst,
    placement: TerminalImagePlacement,
) -> Result<(), LiveSceneError> {
    if placement.generation != pending.generation {
        return Err(LiveSceneError::BoundaryMismatch);
    }
    let definition =
        pending.scene.definition(placement.image_id).ok_or(LiveSceneError::MissingDefinition)?;
    validate_placement(&placement, &definition.metadata)?;

    let key = placement_key(&placement);
    let replacing =
        pending.scene.placements().iter().any(|existing| placement_key(existing) == key);
    if pending.scene.all_placements_len().saturating_add(usize::from(!replacing))
        > ImageLimits::V1.max_placements_per_session as usize
    {
        return Err(LiveSceneError::LimitExceeded(ImageLimitName::PlacementsPerSession));
    }
    let placements = pending.scene.placements_mut();
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
    let PixelRect { x, y, width, height } = placement.source;
    let CellExtent { columns, rows } = placement.destination;
    let source_right = x.checked_add(width).ok_or(LiveSceneError::InvalidPlacement)?;
    let source_bottom = y.checked_add(height).ok_or(LiveSceneError::InvalidPlacement)?;
    if width == 0
        || height == 0
        || columns == 0
        || rows == 0
        || source_right > definition.width
        || source_bottom > definition.height
    {
        return Err(LiveSceneError::InvalidPlacement);
    }
    match placement.kind {
        TerminalImagePlacementKind::KittyUnicodePlaceholder => {
            if placement.protocol != TerminalImageProtocol::Kitty || placement.placeholder.is_none()
            {
                return Err(LiveSceneError::InvalidPlacement);
            }
        }
        TerminalImagePlacementKind::KittyClassic => {
            if placement.protocol != TerminalImageProtocol::Kitty || placement.placeholder.is_some()
            {
                return Err(LiveSceneError::InvalidPlacement);
            }
        }
        TerminalImagePlacementKind::Sixel => {
            if placement.protocol != TerminalImageProtocol::Sixel || placement.placeholder.is_some()
            {
                return Err(LiveSceneError::InvalidPlacement);
            }
        }
    }
    Ok(())
}

fn apply_delete(scene: &mut CommittedImageScene, delete: TerminalImageDelete) {
    let applies = |placement: &TerminalImagePlacement| delete_matches(placement, &delete);
    match delete.scope {
        TerminalImageDeleteScope::Image | TerminalImageDeleteScope::Placement => {
            scene.primary_placements.retain(|placement| !applies(placement));
            scene.alternate_placements.retain(|placement| !applies(placement));
        }
        _ => scene.placements_mut().retain(|placement| !applies(placement)),
    }
    if delete.free_image_data {
        let placed_images = scene
            .primary_placements
            .iter()
            .chain(&scene.alternate_placements)
            .map(|placement| placement.image_id)
            .collect::<Vec<_>>();
        scene.definitions.retain(|definition| {
            let selected = delete.image_id.is_none_or(|id| id == definition.metadata.id);
            !selected || placed_images.contains(&definition.metadata.id)
        });
        scene.retained_rgba_bytes =
            scene.definitions.iter().map(|item| item.metadata.rgba_bytes).sum();
    }
}

fn delete_matches(placement: &TerminalImagePlacement, delete: &TerminalImageDelete) -> bool {
    let image_matches = delete.image_id.is_none_or(|id| placement.image_id == id);
    let placement_matches = delete.placement_id.is_none_or(|id| placement.id == id);
    match delete.scope {
        TerminalImageDeleteScope::AllPlacements => true,
        TerminalImageDeleteScope::Image | TerminalImageDeleteScope::Placement => {
            image_matches && placement_matches
        }
        TerminalImageDeleteScope::Cell => delete.coordinate.is_some_and(|coordinate| {
            placement.anchor.row == coordinate || i32::from(placement.anchor.column) == coordinate
        }),
        TerminalImageDeleteScope::Row => delete.coordinate == Some(placement.anchor.row),
        TerminalImageDeleteScope::Column => {
            delete.coordinate == Some(i32::from(placement.anchor.column))
        }
        TerminalImageDeleteScope::ZIndex => delete.coordinate == Some(placement.z_index),
    }
}

fn apply_grid_effect(scene: &mut CommittedImageScene, effect: &TerminalGridEffect) {
    match *effect {
        TerminalGridEffect::MoveCursor { .. } | TerminalGridEffect::SoftReset => {}
        TerminalGridEffect::Scroll { top, bottom, rows } => {
            let top = i32::from(top);
            let bottom = i32::from(bottom);
            scene.placements_mut().retain_mut(|placement| {
                if !placement.scrolls_with_grid
                    || placement.anchor.row < top
                    || placement.anchor.row > bottom
                {
                    return true;
                }
                placement.anchor.row = placement.anchor.row.saturating_sub(rows);
                placement.anchor.row >= top && placement.anchor.row <= bottom
            });
        }
        TerminalGridEffect::EraseCells { top, left, bottom, right } => {
            scene.placements_mut().retain(|placement| {
                placement.kind != TerminalImagePlacementKind::Sixel
                    || !placement_intersects(placement, top, left, bottom, right)
            });
        }
        TerminalGridEffect::ResizeClip { columns, rows } => {
            scene.placements_mut().retain(|placement| {
                placement.anchor.row >= 0
                    && placement.anchor.row < i32::from(rows)
                    && placement.anchor.column < columns
            });
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

fn placement_intersects(
    placement: &TerminalImagePlacement,
    top: u16,
    left: u16,
    bottom: u16,
    right: u16,
) -> bool {
    let placement_top = placement.anchor.row;
    let placement_left = i32::from(placement.anchor.column);
    let placement_bottom =
        placement_top.saturating_add(i32::from(placement.destination.rows)).saturating_sub(1);
    let placement_right =
        placement_left.saturating_add(i32::from(placement.destination.columns)).saturating_sub(1);
    placement_top <= i32::from(bottom)
        && placement_bottom >= i32::from(top)
        && placement_left <= i32::from(right)
        && placement_right >= i32::from(left)
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
