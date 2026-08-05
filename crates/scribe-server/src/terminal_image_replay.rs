//! Plan one generation-tagged combined image replay for a session snapshot.
//!
//! [`crate::terminal_image_publication`] turns an incremental committed read
//! into live records. This module answers the other question: what does a sink
//! that has *no* scene — a late attacher, or one whose backlog was shed — need
//! in order to converge on the session's canonical state right now?
//!
//! The answer is one bounded burst: `Begin`, every surviving definition with
//! its RGBA split into wire-sized chunks, every surviving placement tagged with
//! its owning screen, and `Commit`. Every record carries the same generation
//! and the snapshot's output cursor, so a receiver stages the whole burst and
//! swaps at `Commit` — a partial scene is never observable.
//!
//! The burst is planned once per recovery and fanned out to however many sinks
//! owe it, so the server never retains a per-sink copy of the scene. Canonical
//! pixels arrive through the same payload seam the live path uses: a definition
//! the caller cannot pay for is withdrawn along with every placement naming it,
//! because an unbacked definition would leave the receiver staging a scene it
//! can never complete.

use scribe_common::terminal_images::{
    BoundedImageBytes, ImageLimits, TerminalImageDataChunk, TerminalImageDefinition,
    TerminalImageGeneration, TerminalImagePlacement, TerminalImageReplayMessage,
    TerminalOutputSequence, TerminalScreenKind,
};

use crate::terminal_image_publication::DefinitionPayload;

/// The canonical snapshot one replay burst describes.
pub struct ReplayInputs<'a> {
    /// Generation every record in the burst is tagged with.
    pub generation: TerminalImageGeneration,
    /// Output cursor the snapshot reflects. Live records at or below it are
    /// already in the snapshot; later ones buffer behind the commit.
    pub through_sequence: TerminalOutputSequence,
    /// Grid the snapshot leaves active.
    pub active_screen: TerminalScreenKind,
    /// Canonical definitions, in identifier order.
    pub definitions: &'a [TerminalImageDefinition],
    /// Canonical placements with their owning screens.
    pub placements: &'a [(TerminalScreenKind, TerminalImagePlacement)],
}

/// Payload-free facts about one planned burst, for diagnostics and evidence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplayPlanCounters {
    /// Definitions the burst carries complete RGBA for.
    pub definitions: u32,
    /// Placements the burst restates.
    pub placements: u32,
    /// Total canonical RGBA bytes across every carried definition.
    pub total_rgba_bytes: u64,
    /// Data chunks the burst splits those bytes into.
    pub chunks: u32,
    /// Definitions dropped because the caller could not supply their pixels.
    pub withdrawn_definitions: u32,
    /// Placements dropped because their definition was withdrawn or stale.
    pub withdrawn_placements: u32,
    /// Largest single record payload, which must stay within the wire chunk cap.
    pub max_chunk_bytes: u64,
}

/// One planned burst: the ordered records plus what they cost.
pub struct ReplayPlan {
    pub records: Vec<TerminalImageReplayMessage>,
    pub counters: ReplayPlanCounters,
}

/// Plan the bounded replay burst for `inputs`.
///
/// The burst always has a `Begin` and a `Commit`, so an empty scene is still a
/// truthful two-record statement — that is what converges a receiver holding
/// stale placements after its output was shed.
// @lat: [[terminal-images#Terminal Images#Combined Image Replay]]
#[must_use]
pub fn plan_replay(inputs: &ReplayInputs<'_>, payload: DefinitionPayload<'_>) -> ReplayPlan {
    let generation = inputs.generation;
    let mut counters = ReplayPlanCounters::default();
    let mut body: Vec<TerminalImageReplayMessage> = Vec::new();
    let mut carried: Vec<scribe_common::terminal_images::TerminalImageId> = Vec::new();

    for definition in inputs.definitions {
        // A definition from an older generation belongs to a scene this
        // snapshot has already replaced; carrying it would resurrect it.
        if definition.generation != generation {
            counters.withdrawn_definitions += 1;
            continue;
        }
        let Some(rgba) =
            payload(definition).filter(|rgba| rgba.len() as u64 == definition.rgba_bytes)
        else {
            counters.withdrawn_definitions += 1;
            continue;
        };
        let Some(chunks) = chunk_definition(generation, definition, &rgba) else {
            counters.withdrawn_definitions += 1;
            continue;
        };
        carried.push(definition.id);
        counters.definitions += 1;
        counters.total_rgba_bytes = counters.total_rgba_bytes.saturating_add(definition.rgba_bytes);
        counters.chunks += u32::try_from(chunks.len()).unwrap_or(u32::MAX);
        body.push(TerminalImageReplayMessage::Definition {
            generation,
            definition: definition.clone(),
        });
        body.extend(chunks);
    }

    for (screen, placement) in inputs.placements {
        if placement.generation != generation || !carried.contains(&placement.image_id) {
            counters.withdrawn_placements += 1;
            continue;
        }
        counters.placements += 1;
        body.push(TerminalImageReplayMessage::Placement {
            generation,
            placement: placement.clone(),
            screen: Some(*screen),
        });
    }

    counters.max_chunk_bytes = body
        .iter()
        .map(|record| match record {
            TerminalImageReplayMessage::DefinitionChunk { chunk, .. } => chunk.data.len() as u64,
            _ => 0,
        })
        .max()
        .unwrap_or(0);

    let mut records = Vec::with_capacity(body.len() + 2);
    records.push(TerminalImageReplayMessage::Begin {
        generation,
        after_sequence: inputs.through_sequence,
        definition_count: counters.definitions,
        placement_count: counters.placements,
        total_rgba_bytes: counters.total_rgba_bytes,
        active_screen: Some(inputs.active_screen),
    });
    records.append(&mut body);
    records.push(TerminalImageReplayMessage::Commit {
        generation,
        through_sequence: inputs.through_sequence,
    });
    ReplayPlan { records, counters }
}

/// Split one definition's canonical RGBA into wire-sized chunks.
///
/// `None` withdraws the definition: the only way to get there is a chunk that
/// will not fit the bound, which `chunks` already guarantees it cannot.
fn chunk_definition(
    generation: TerminalImageGeneration,
    definition: &TerminalImageDefinition,
    rgba: &[u8],
) -> Option<Vec<TerminalImageReplayMessage>> {
    // The contract's replay chunk ceiling and the wire type's bound are one
    // number; the bound is what actually rejects an oversized chunk.
    debug_assert_eq!(ImageLimits::V1.max_replay_chunk_bytes, BoundedImageBytes::MAX_LEN as u64);
    let limit = BoundedImageBytes::MAX_LEN;
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    for slice in rgba.chunks(limit) {
        let end = offset.saturating_add(slice.len() as u64);
        let data = BoundedImageBytes::new(slice.to_vec()).ok()?;
        chunks.push(TerminalImageReplayMessage::DefinitionChunk {
            generation,
            chunk: TerminalImageDataChunk {
                id: definition.id,
                generation,
                offset,
                data,
                final_chunk: end == definition.rgba_bytes,
            },
        });
        offset = end;
    }
    Some(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::terminal_images::{
        CellExtent, PixelRect, TerminalCellAnchor, TerminalImageId, TerminalImagePlacementKind,
        TerminalImageProtocol, TerminalPlacementId,
    };

    const GENERATION: TerminalImageGeneration = TerminalImageGeneration(7);

    fn definition(id: u32, width: u32, height: u32) -> TerminalImageDefinition {
        TerminalImageDefinition::new(
            TerminalImageId(u64::from(id)),
            GENERATION,
            width,
            height,
            true,
        )
        .expect("bounded definition")
    }

    fn placement(image: u32, id: u32) -> TerminalImagePlacement {
        TerminalImagePlacement {
            id: TerminalPlacementId(u64::from(id)),
            image_id: TerminalImageId(u64::from(image)),
            generation: GENERATION,
            protocol: TerminalImageProtocol::Kitty,
            kind: TerminalImagePlacementKind::KittyClassic,
            anchor: TerminalCellAnchor { row: 0, column: 0 },
            source: PixelRect { x: 0, y: 0, width: 1, height: 1 },
            destination: CellExtent { columns: 1, rows: 1 },
            pixel_offset_x: 0,
            pixel_offset_y: 0,
            z_index: 0,
            scrolls_with_grid: true,
            move_cursor: false,
            cell_clip: None,
            placeholder: None,
        }
    }

    fn full_payload(definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
        Some(vec![7u8; usize::try_from(definition.rgba_bytes).ok()?])
    }

    #[test]
    fn a_maximum_scene_plans_only_wire_sized_chunks() {
        // The largest image v1 admits, repeated until the session retention cap
        // is reached — the worst case the replay path must stay bounded under.
        let limits = ImageLimits::V1;
        let each = limits
            .canonical_rgba_len(limits.max_width_pixels, limits.max_height_pixels)
            .expect("maximum canonical length");
        let count = u32::try_from(limits.max_session_retained_cpu_bytes / each)
            .expect("bounded image count");
        assert!(count > 0 && count <= limits.max_images_per_session);
        let definitions: Vec<_> = (1..=count)
            .map(|id| definition(id, limits.max_width_pixels, limits.max_height_pixels))
            .collect();
        let placements: Vec<_> =
            (1..=count).map(|id| (TerminalScreenKind::Primary, placement(id, id))).collect();

        let plan = plan_replay(
            &ReplayInputs {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(42),
                active_screen: TerminalScreenKind::Primary,
                definitions: &definitions,
                placements: &placements,
            },
            &mut full_payload,
        );

        assert_eq!(plan.counters.definitions, count);
        assert_eq!(plan.counters.placements, count);
        assert_eq!(plan.counters.total_rgba_bytes, each * u64::from(count));
        assert!(plan.counters.total_rgba_bytes <= limits.max_session_retained_cpu_bytes);
        assert!(plan.counters.max_chunk_bytes <= limits.max_replay_chunk_bytes);
        // Exactly enough chunks to carry the bytes, none of them oversized.
        assert_eq!(
            u64::from(plan.counters.chunks),
            plan.counters.total_rgba_bytes.div_ceil(limits.max_replay_chunk_bytes)
        );
        for record in &plan.records {
            assert!(record.validate().is_ok(), "planned record must validate");
        }
    }

    #[test]
    fn the_burst_opens_and_closes_on_one_generation_and_cursor() {
        let definitions = vec![definition(1, 2, 2)];
        let placements = vec![(TerminalScreenKind::Alternate, placement(1, 1))];
        let plan = plan_replay(
            &ReplayInputs {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(9),
                active_screen: TerminalScreenKind::Alternate,
                definitions: &definitions,
                placements: &placements,
            },
            &mut full_payload,
        );

        assert!(matches!(
            plan.records.first(),
            Some(TerminalImageReplayMessage::Begin {
                generation: GENERATION,
                after_sequence: TerminalOutputSequence(9),
                definition_count: 1,
                placement_count: 1,
                active_screen: Some(TerminalScreenKind::Alternate),
                ..
            })
        ));
        assert!(matches!(
            plan.records.last(),
            Some(TerminalImageReplayMessage::Commit {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(9),
            })
        ));
        assert!(matches!(
            plan.records
                .iter()
                .find(|record| matches!(record, TerminalImageReplayMessage::Placement { .. })),
            Some(TerminalImageReplayMessage::Placement {
                screen: Some(TerminalScreenKind::Alternate),
                ..
            })
        ));
    }

    #[test]
    fn an_unpayable_definition_withdraws_its_placements() {
        let definitions = vec![definition(1, 2, 2), definition(2, 2, 2)];
        let placements = vec![
            (TerminalScreenKind::Primary, placement(1, 1)),
            (TerminalScreenKind::Primary, placement(2, 2)),
        ];
        let plan = plan_replay(
            &ReplayInputs {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(1),
                active_screen: TerminalScreenKind::Primary,
                definitions: &definitions,
                placements: &placements,
            },
            &mut |definition| (definition.id.0 == 1).then(|| full_payload(definition))?,
        );

        assert_eq!(plan.counters.definitions, 1);
        assert_eq!(plan.counters.withdrawn_definitions, 1);
        assert_eq!(plan.counters.placements, 1);
        assert_eq!(plan.counters.withdrawn_placements, 1);
        assert!(!plan.records.iter().any(|record| matches!(
            record,
            TerminalImageReplayMessage::Placement { placement, .. }
                if placement.image_id == TerminalImageId(2)
        )));
    }

    #[test]
    fn an_empty_scene_is_still_a_truthful_two_record_burst() {
        let plan = plan_replay(
            &ReplayInputs {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(3),
                active_screen: TerminalScreenKind::Primary,
                definitions: &[],
                placements: &[],
            },
            &mut full_payload,
        );
        assert_eq!(plan.records.len(), 2);
        assert_eq!(
            plan.counters,
            ReplayPlanCounters { max_chunk_bytes: 0, ..ReplayPlanCounters::default() }
        );
    }

    #[test]
    fn a_stale_generation_never_survives_the_snapshot() {
        let mut stale = definition(1, 2, 2);
        stale.generation = TerminalImageGeneration(GENERATION.0 - 1);
        let placements = vec![(TerminalScreenKind::Primary, placement(1, 1))];
        let plan = plan_replay(
            &ReplayInputs {
                generation: GENERATION,
                through_sequence: TerminalOutputSequence(1),
                active_screen: TerminalScreenKind::Primary,
                definitions: std::slice::from_ref(&stale),
                placements: &placements,
            },
            &mut full_payload,
        );
        assert_eq!(plan.counters.definitions, 0);
        assert_eq!(plan.counters.withdrawn_definitions, 1);
        assert_eq!(plan.counters.withdrawn_placements, 1);
        assert_eq!(plan.records.len(), 2);
    }
}
