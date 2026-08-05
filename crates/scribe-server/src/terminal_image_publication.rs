//! Translate committed canonical mutations into ordered client live records.
//!
//! The session seam owns canonical image state; a connected client owns a
//! convergent copy of it. This module is the only place the two are related:
//! it turns one committed [`MutationLog`] into generation- and
//! sequence-tagged [`TerminalImageLiveMessage`] bursts whose replay on
//! `LiveImageScene` reproduces the server's definitions, placements, screen
//! ownership, and counters exactly.
//!
//! Every placement record names its owning screen explicitly, so a mutation
//! on the inactive grid — resize clipping, eviction, a screen switch — cannot
//! land in the wrong client bucket.

use std::collections::BTreeSet;

use scribe_common::terminal_images::{
    BoundedImageBytes, TerminalGridEffect, TerminalImageDataChunk, TerminalImageDefinition,
    TerminalImageDelete, TerminalImageDeleteScope, TerminalImageGeneration, TerminalImageId,
    TerminalImageLiveMessage, TerminalImagePlacement, TerminalImageRejection, TerminalImageUpdate,
    TerminalOutputSequence, TerminalScreenKind,
};

use crate::terminal_image_mutations::CanonicalImageMutation;

/// Canonical RGBA bytes for one published definition.
///
/// The seam is payload-free, so the caller that owns decoded pixels supplies
/// them. Returning `None` withdraws the definition and every placement naming
/// it from this burst rather than publishing a record the client cannot apply.
pub type DefinitionPayload<'a> = &'a mut dyn FnMut(&TerminalImageDefinition) -> Option<Vec<u8>>;

/// One generation-consistent group of updates awaiting a sequence number.
struct Burst {
    generation: TerminalImageGeneration,
    defined: BTreeSet<TerminalImageId>,
    withdrawn: BTreeSet<TerminalImageId>,
    updates: Vec<TerminalImageUpdate>,
}

impl Burst {
    fn new(generation: TerminalImageGeneration) -> Self {
        Self {
            generation,
            defined: BTreeSet::new(),
            withdrawn: BTreeSet::new(),
            updates: Vec::new(),
        }
    }
}

/// Everything one committed read needs to publish, gathered by the seam.
pub struct PublicationInputs<'a> {
    /// Generation in force before the read was committed.
    pub start_generation: TerminalImageGeneration,
    /// Generation in force after the read was committed.
    pub end_generation: TerminalImageGeneration,
    /// Active screen before the read was committed.
    pub start_screen: TerminalScreenKind,
    /// Active screen after the read was committed.
    pub end_screen: TerminalScreenKind,
    /// Ordered canonical mutations the read committed.
    pub mutations: &'a [CanonicalImageMutation],
    /// Canonical placements surviving the read, in screen/image/placement order.
    pub placements: &'a [(TerminalScreenKind, TerminalImagePlacement)],
}

/// Build the ordered live records for one committed read.
///
/// Sequence numbers are assigned consecutively starting at `first_sequence`;
/// the returned count is how many the caller must charge to its cursor. A read
/// with nothing to say returns no messages and consumes no sequence.
pub fn publish(
    inputs: &PublicationInputs<'_>,
    first_sequence: TerminalOutputSequence,
    payload: DefinitionPayload<'_>,
) -> (Vec<TerminalImageLiveMessage>, u64) {
    let mut bursts = plan(inputs, payload);
    if inputs.end_screen != inputs.start_screen {
        // The screen switch is the last thing the client learns, so every
        // screen-scoped record above it already landed in its own bucket.
        if bursts.last().is_none_or(|last| last.generation != inputs.end_generation) {
            bursts.push(Burst::new(inputs.end_generation));
        }
        if let Some(last) = bursts.last_mut() {
            last.updates.push(TerminalImageUpdate::GridEffect {
                effect: TerminalGridEffect::SwitchScreen { screen: inputs.end_screen },
            });
        }
    }
    bursts.retain(|burst| !burst.updates.is_empty());

    let mut messages = Vec::new();
    let mut sequence = first_sequence.0;
    for burst in &bursts {
        let generation = burst.generation;
        let tag = TerminalOutputSequence(sequence);
        messages.push(TerminalImageLiveMessage::Begin { generation, sequence: tag });
        for update in &burst.updates {
            messages.push(TerminalImageLiveMessage::Update {
                generation,
                sequence: tag,
                update: update.clone(),
            });
        }
        messages.push(TerminalImageLiveMessage::Commit { generation, sequence: tag });
        sequence = sequence.saturating_add(1);
    }
    (messages, sequence.saturating_sub(first_sequence.0))
}

/// Group the committed mutations into generation-consistent bursts.
///
/// A hard reset opens a new generation mid-read, and the client binds every
/// record in one burst to a single generation, so the burst boundary follows
/// the generation carried by each definition and placement.
fn plan(inputs: &PublicationInputs<'_>, payload: DefinitionPayload<'_>) -> Vec<Burst> {
    let mut bursts: Vec<Burst> = Vec::new();
    let mut current = Burst::new(inputs.start_generation);
    for mutation in inputs.mutations {
        if let Some(generation) = mutation_generation(mutation)
            && generation != current.generation
        {
            bursts.push(std::mem::replace(&mut current, Burst::new(generation)));
        }
        translate(mutation, &mut current, payload);
    }
    bursts.push(current);
    republish_redefined_placements(inputs, &mut bursts);
    bursts
}

/// Restate the placements a redefinition displaced on the client.
///
/// A completed definition replaces the client's image data and drops the
/// placements bound to the previous bytes. The server keeps those placements,
/// so the burst that redefined an image ends by restating whichever of them
/// still exist canonically.
fn republish_redefined_placements(inputs: &PublicationInputs<'_>, bursts: &mut [Burst]) {
    let Some(last) = bursts.last_mut() else { return };
    if last.defined.is_empty() {
        return;
    }
    for (screen, placement) in inputs.placements {
        if !last.defined.contains(&placement.image_id)
            || placement.generation != last.generation
            || last.updates.iter().any(|update| {
                matches!(update, TerminalImageUpdate::Place { placement: published, .. }
                    if published.image_id == placement.image_id && published.id == placement.id)
            })
        {
            continue;
        }
        last.updates.push(TerminalImageUpdate::Place {
            placement: placement.clone(),
            screen: Some(*screen),
        });
    }
}

/// Generation a mutation is tagged with, when it carries one.
const fn mutation_generation(mutation: &CanonicalImageMutation) -> Option<TerminalImageGeneration> {
    match mutation {
        CanonicalImageMutation::Define { definition } => Some(definition.generation),
        CanonicalImageMutation::Place { placement, .. } => Some(placement.generation),
        CanonicalImageMutation::RemovePlacement { .. }
        | CanonicalImageMutation::FreeImage { .. }
        | CanonicalImageMutation::Reject { .. } => None,
    }
}

fn translate(mutation: &CanonicalImageMutation, burst: &mut Burst, payload: DefinitionPayload<'_>) {
    match mutation {
        CanonicalImageMutation::Define { definition } => define(definition, burst, payload),
        CanonicalImageMutation::Place { screen, placement } => {
            if burst.withdrawn.contains(&placement.image_id) {
                return;
            }
            burst.updates.push(TerminalImageUpdate::Place {
                placement: placement.clone(),
                screen: Some(*screen),
            });
        }
        CanonicalImageMutation::RemovePlacement { screen, image_id, placement_id, .. } => {
            burst.updates.push(TerminalImageUpdate::Delete {
                delete: TerminalImageDelete {
                    scope: TerminalImageDeleteScope::Placement,
                    image_id: Some(*image_id),
                    placement_id: Some(*placement_id),
                    coordinate: None,
                    free_image_data: false,
                },
                screen: Some(*screen),
            });
        }
        CanonicalImageMutation::FreeImage { image_id, .. } => {
            burst.updates.push(TerminalImageUpdate::Delete {
                delete: TerminalImageDelete {
                    scope: TerminalImageDeleteScope::Image,
                    image_id: Some(*image_id),
                    placement_id: None,
                    coordinate: None,
                    free_image_data: true,
                },
                screen: None,
            });
        }
        CanonicalImageMutation::Reject { reason } => {
            burst.updates.push(TerminalImageUpdate::Rejected {
                rejection: TerminalImageRejection {
                    reason: *reason,
                    protocol: None,
                    action: None,
                    width: None,
                    height: None,
                    observed: None,
                    limit: None,
                },
            });
        }
    }
}

fn define(definition: &TerminalImageDefinition, burst: &mut Burst, payload: DefinitionPayload<'_>) {
    let Some(rgba) = payload(definition).filter(|rgba| rgba.len() as u64 == definition.rgba_bytes)
    else {
        burst.withdrawn.insert(definition.id);
        return;
    };
    burst.withdrawn.remove(&definition.id);
    burst.defined.insert(definition.id);
    burst.updates.push(TerminalImageUpdate::Define { definition: definition.clone() });
    let mut offset = 0u64;
    for slice in rgba.chunks(BoundedImageBytes::MAX_LEN) {
        let end = offset.saturating_add(slice.len() as u64);
        let Ok(data) = BoundedImageBytes::new(slice.to_vec()) else {
            // `chunks` never exceeds the bound, so this is unreachable.
            burst.updates.pop();
            burst.defined.remove(&definition.id);
            burst.withdrawn.insert(definition.id);
            return;
        };
        burst.updates.push(TerminalImageUpdate::DefinitionChunk {
            chunk: TerminalImageDataChunk {
                id: definition.id,
                generation: definition.generation,
                offset,
                data,
                final_chunk: end == definition.rgba_bytes,
            },
        });
        offset = end;
    }
}
