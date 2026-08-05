//! Canonical definition and placement state for one terminal session.
//!
//! This module owns the state half of transactional image mutation. The
//! ordering seam clones a [`CanonicalImageState`], applies a whole read to the
//! clone, and swaps it in only after every mutation succeeds, so a quota,
//! validation, or protocol failure leaves the prior committed state and its
//! counters exactly as they were.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use scribe_common::terminal_images::{
    CellExtent, ImageLimits, PixelRect, PlaceholderMetadata, TerminalCellAnchor,
    TerminalImageCellClip, TerminalImageDefinition, TerminalImageDelete, TerminalImageDeleteScope,
    TerminalImageGeneration, TerminalImageId, TerminalImagePlacement, TerminalImagePlacementKind,
    TerminalImageProtocol, TerminalImageRejectionReason, TerminalPlacementId, TerminalScreenKind,
};
use scribe_pty::graphics_framing::{
    GraphicsStorageBudget, GraphicsStorageClass, GraphicsStorageRejection, GraphicsStorageVec,
    KittyAction, KittyCommand, KittyPlacementMode,
};

/// Result of one ledger-charged canonical mutation.
pub type MutationResult = Result<(), GraphicsStorageRejection>;

/// Ordered, ledger-charged canonical mutations for one transactional phase.
///
/// Grid effects can republish up to every live placement, so the mutation list
/// is hostile-input proportional and reserves each entry before retaining it.
#[derive(Debug)]
pub struct MutationLog {
    entries: GraphicsStorageVec<CanonicalImageMutation>,
}

impl MutationLog {
    /// Allocate one charged log against the session/process ledger pair.
    pub fn new(budget: Arc<GraphicsStorageBudget>) -> Result<Self, GraphicsStorageRejection> {
        let entries = GraphicsStorageVec::new(budget, GraphicsStorageClass::CanonicalMutations)?;
        Ok(Self { entries })
    }

    fn push(&mut self, mutation: CanonicalImageMutation) -> MutationResult {
        self.entries.push(mutation)
    }

    fn reject(&mut self, reason: TerminalImageRejectionReason) -> MutationResult {
        self.push(CanonicalImageMutation::Reject { reason })
    }

    /// Borrow the ordered mutations while their storage ownership lives.
    #[must_use]
    pub fn as_slice(&self) -> &[CanonicalImageMutation] {
        self.entries.as_slice()
    }
}

/// Ordered canonical mutation published to accounting, evidence, and clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalImageMutation {
    /// A definition was created or replaced under the current generation.
    Define { definition: TerminalImageDefinition },
    /// A placement was created, replaced, or moved on exactly one screen.
    Place { screen: TerminalScreenKind, placement: TerminalImagePlacement },
    /// One exact placement identity left canonical state.
    RemovePlacement {
        screen: TerminalScreenKind,
        image_id: TerminalImageId,
        placement_id: TerminalPlacementId,
        reason: PlacementRemoval,
    },
    /// One definition's canonical bytes are no longer referenced.
    FreeImage { image_id: TerminalImageId, evicted: bool },
    /// A protocol-level failure that changed nothing.
    Reject { reason: TerminalImageRejectionReason },
}

/// Why an exact placement identity was removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementRemoval {
    /// An explicit Kitty delete command named this placement.
    Deleted,
    /// A terminal erase, scroll, resize, screen, or reset effect dropped it.
    GridEffect,
    /// A session quota forced deterministic eviction.
    Evicted,
}

/// One definition plus the monotonic tick that fixes its eviction order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalDefinition {
    definition: TerminalImageDefinition,
    defined_at: u64,
}

/// One placement plus the monotonic tick that fixes its eviction order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalPlacement {
    placement: TerminalImagePlacement,
    placed_at: u64,
}

/// Exact protocol identity of one placement, scoped to its owning screen.
type PlacementKey = (TerminalScreenKind, TerminalImageId, TerminalPlacementId);

/// Terminal facts a mutation needs from the production Alacritty observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MutationContext {
    pub screen: TerminalScreenKind,
    pub cursor_row: i32,
    pub cursor_column: u16,
    pub cell_width_pixels: u16,
    pub cell_height_pixels: u16,
}

/// Payload-free decoded canonical facts carried by a completed boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedImageMeta {
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
}

/// Canonical, generation-tagged image state for one session.
// @lat: [[terminal-images#Terminal Images#Transactional Image Mutations]]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalImageState {
    limits: ImageLimits,
    generation: TerminalImageGeneration,
    active_screen: TerminalScreenKind,
    definitions: BTreeMap<TerminalImageId, CanonicalDefinition>,
    placements: BTreeMap<PlacementKey, CanonicalPlacement>,
    /// Monotonic tick making eviction order deterministic and independent of
    /// identifier values chosen by a hostile application.
    tick: u64,
    /// Next server-assigned identifier for images the application did not name.
    next_assigned_image_id: u64,
}

impl CanonicalImageState {
    /// Empty state under the session's immutable limits.
    #[must_use]
    pub fn new(limits: ImageLimits) -> Self {
        Self {
            limits,
            generation: TerminalImageGeneration(1),
            active_screen: TerminalScreenKind::Primary,
            definitions: BTreeMap::new(),
            placements: BTreeMap::new(),
            tick: 0,
            // Server-assigned identifiers start above the Kitty `i=` range so
            // they can never collide with an application-chosen identifier.
            next_assigned_image_id: u64::from(u32::MAX) + 1,
        }
    }

    #[must_use]
    pub fn definition_count(&self) -> usize {
        self.definitions.len()
    }

    #[must_use]
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    #[must_use]
    pub fn active_screen(&self) -> TerminalScreenKind {
        self.active_screen
    }

    pub fn set_active_screen(&mut self, screen: TerminalScreenKind) {
        self.active_screen = screen;
    }

    pub fn set_generation(&mut self, generation: TerminalImageGeneration) {
        self.generation = generation;
    }

    /// Generation every definition and placement committed here is tagged with.
    #[must_use]
    pub const fn generation(&self) -> TerminalImageGeneration {
        self.generation
    }

    /// Payload-free canonical definitions in identifier order.
    #[must_use]
    pub fn definitions(&self) -> Vec<TerminalImageDefinition> {
        self.definitions.values().map(|entry| entry.definition.clone()).collect()
    }

    /// Payload-free canonical placements in screen/image/placement order.
    #[must_use]
    pub fn placements(&self) -> Vec<(TerminalScreenKind, TerminalImagePlacement)> {
        self.placements.iter().map(|(key, entry)| (key.0, entry.placement.clone())).collect()
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }

    /// Apply one completed Kitty boundary and record its ordered mutations.
    pub fn apply_kitty(
        &mut self,
        command: &KittyCommand,
        decoded: Option<DecodedImageMeta>,
        context: MutationContext,
        out: &mut MutationLog,
    ) -> MutationResult {
        match command.action {
            KittyAction::Query => Ok(()),
            KittyAction::Transmit | KittyAction::TransmitDisplay => {
                let Some(decoded) = decoded else { return Ok(()) };
                let image_id =
                    command.image_id.map_or(TerminalImageId(self.next_assigned_image_id), |id| {
                        TerminalImageId(u64::from(id))
                    });
                let Ok(definition) = TerminalImageDefinition::new(
                    image_id,
                    self.generation,
                    decoded.width,
                    decoded.height,
                    decoded.has_alpha,
                ) else {
                    return out.reject(TerminalImageRejectionReason::InvalidDimensions);
                };
                // A compound transmit-and-display validates both halves before
                // either is committed, so a bad placement never leaves a
                // half-applied definition behind.
                let placement = if command.action == KittyAction::TransmitDisplay {
                    match kitty_placement(command, &definition, self.generation, context)
                        .and_then(|placement| validated(placement, &definition))
                    {
                        Ok(placement) => Some(placement),
                        Err(reason) => return out.reject(reason),
                    }
                } else {
                    None
                };
                self.commit_definition(definition, command.image_id.is_none(), out)?;
                placement.map_or(Ok(()), |placement| {
                    self.insert_placement(placement, context.screen, out)
                })
            }
            KittyAction::Put => {
                let Some(image_id) = command.image_id.map(|id| TerminalImageId(u64::from(id)))
                else {
                    return out.reject(TerminalImageRejectionReason::ImageNotFound);
                };
                if !self.definitions.contains_key(&image_id) {
                    return out.reject(TerminalImageRejectionReason::ImageNotFound);
                }
                self.place_kitty(command, image_id, context, out)
            }
            KittyAction::Delete => self.apply_delete(kitty_delete(command), out),
        }
    }

    /// Apply one decoded Sixel image: define it and place it at the cursor.
    pub fn apply_sixel(
        &mut self,
        decoded: DecodedImageMeta,
        context: MutationContext,
        out: &mut MutationLog,
    ) -> MutationResult {
        let image_id = TerminalImageId(self.next_assigned_image_id);
        let Ok(definition) = TerminalImageDefinition::new(
            image_id,
            self.generation,
            decoded.width,
            decoded.height,
            decoded.has_alpha,
        ) else {
            return out.reject(TerminalImageRejectionReason::InvalidDimensions);
        };
        let source = PixelRect { x: 0, y: 0, width: definition.width, height: definition.height };
        let Some(destination) = cell_extent(source.width, source.height, context) else {
            return out.reject(TerminalImageRejectionReason::InvalidDimensions);
        };
        let placement = TerminalImagePlacement {
            id: TerminalPlacementId(0),
            image_id,
            generation: self.generation,
            protocol: TerminalImageProtocol::Sixel,
            kind: TerminalImagePlacementKind::Sixel,
            anchor: TerminalCellAnchor { row: context.cursor_row, column: context.cursor_column },
            source,
            destination,
            pixel_offset_x: 0,
            pixel_offset_y: 0,
            z_index: 0,
            scrolls_with_grid: true,
            move_cursor: true,
            cell_clip: None,
            placeholder: None,
        };
        let placement = match validated(placement, &definition) {
            Ok(placement) => placement,
            Err(reason) => return out.reject(reason),
        };
        self.commit_definition(definition, true, out)?;
        self.insert_placement(placement, context.screen, out)
    }

    /// Commit one validated definition, evicting the oldest images first.
    fn commit_definition(
        &mut self,
        definition: TerminalImageDefinition,
        assigned: bool,
        out: &mut MutationLog,
    ) -> MutationResult {
        let image_id = definition.id;
        if !self.definitions.contains_key(&image_id) {
            self.evict_definitions_for_one_more(out)?;
        }
        if assigned {
            self.next_assigned_image_id = self.next_assigned_image_id.saturating_add(1);
        }
        let defined_at = self.next_tick();
        self.definitions
            .insert(image_id, CanonicalDefinition { definition: definition.clone(), defined_at });
        out.push(CanonicalImageMutation::Define { definition })
    }

    fn place_kitty(
        &mut self,
        command: &KittyCommand,
        image_id: TerminalImageId,
        context: MutationContext,
        out: &mut MutationLog,
    ) -> MutationResult {
        let Some(definition) =
            self.definitions.get(&image_id).map(|entry| entry.definition.clone())
        else {
            return out.reject(TerminalImageRejectionReason::ImageNotFound);
        };
        match kitty_placement(command, &definition, self.generation, context)
            .and_then(|placement| validated(placement, &definition))
        {
            Ok(placement) => self.insert_placement(placement, context.screen, out),
            Err(reason) => out.reject(reason),
        }
    }

    /// Commit one validated placement, evicting the oldest placements first.
    fn insert_placement(
        &mut self,
        placement: TerminalImagePlacement,
        screen: TerminalScreenKind,
        out: &mut MutationLog,
    ) -> MutationResult {
        let key = (screen, placement.image_id, placement.id);
        if !self.placements.contains_key(&key) {
            self.evict_placements_for_one_more(out)?;
        }
        let placed_at = self.next_tick();
        self.placements.insert(key, CanonicalPlacement { placement: placement.clone(), placed_at });
        out.push(CanonicalImageMutation::Place { screen, placement })
    }

    /// Apply one exact protocol delete across its own scope.
    ///
    /// Identity and placement scopes reach both screens, matching Kitty's
    /// image-global delete semantics; every geometric scope stays on the
    /// active screen only.
    pub fn apply_delete(
        &mut self,
        delete: TerminalImageDelete,
        out: &mut MutationLog,
    ) -> MutationResult {
        let both_screens = matches!(
            delete.scope,
            TerminalImageDeleteScope::Image | TerminalImageDeleteScope::Placement
        );
        let active = self.active_screen;
        let doomed: Vec<PlacementKey> = self
            .placements
            .iter()
            .filter(|(key, entry)| {
                (both_screens || key.0 == active) && entry.placement.matches_delete(&delete)
            })
            .map(|(key, _)| *key)
            .collect();
        let mut freed: BTreeSet<TerminalImageId> = BTreeSet::new();
        for key in doomed {
            freed.insert(key.1);
            self.remove_placement(key, PlacementRemoval::Deleted, out)?;
        }
        if !delete.free_image_data {
            return Ok(());
        }
        if delete.scope == TerminalImageDeleteScope::Image {
            freed.extend(delete.image_id);
        }
        let still_placed: BTreeSet<TerminalImageId> =
            self.placements.keys().map(|key| key.1).collect();
        for image_id in freed {
            if still_placed.contains(&image_id) {
                continue;
            }
            if self.definitions.remove(&image_id).is_some() {
                out.push(CanonicalImageMutation::FreeImage { image_id, evicted: false })?;
            }
        }
        Ok(())
    }

    fn remove_placement(
        &mut self,
        key: PlacementKey,
        reason: PlacementRemoval,
        out: &mut MutationLog,
    ) -> MutationResult {
        if self.placements.remove(&key).is_none() {
            return Ok(());
        }
        out.push(CanonicalImageMutation::RemovePlacement {
            screen: key.0,
            image_id: key.1,
            placement_id: key.2,
            reason,
        })
    }

    /// Remove every visible placement on one screen (ED2 and 1049 creation).
    pub fn clear_screen(
        &mut self,
        screen: TerminalScreenKind,
        out: &mut MutationLog,
    ) -> MutationResult {
        let doomed: Vec<PlacementKey> =
            self.placements.keys().copied().filter(|key| key.0 == screen).collect();
        for key in doomed {
            self.remove_placement(key, PlacementRemoval::GridEffect, out)?;
        }
        Ok(())
    }

    /// Drop every definition and placement (RIS and equivalent hard resets).
    ///
    /// A reset invalidates the whole scene, so it also opens the next
    /// generation. Callers preflight generation headroom before mutating; an
    /// exhausted counter here can only mean that preflight was skipped.
    pub fn reset(&mut self, out: &mut MutationLog) -> MutationResult {
        let next_generation =
            self.generation.0.checked_add(1).ok_or(GraphicsStorageRejection::InternalInvariant)?;
        let doomed: Vec<PlacementKey> = self.placements.keys().copied().collect();
        for key in doomed {
            self.remove_placement(key, PlacementRemoval::GridEffect, out)?;
        }
        let images: Vec<TerminalImageId> = self.definitions.keys().copied().collect();
        self.definitions.clear();
        for image_id in images {
            out.push(CanonicalImageMutation::FreeImage { image_id, evicted: false })?;
        }
        self.active_screen = TerminalScreenKind::Primary;
        self.generation = TerminalImageGeneration(next_generation);
        Ok(())
    }

    /// Drop Sixel placements overlapping one half-open erase rectangle.
    ///
    /// Kitty graphics are independent of ordinary text erases, so classic and
    /// placeholder placements survive every erase short of ED2 or reset.
    pub fn erase_cells(
        &mut self,
        screen: TerminalScreenKind,
        area: TerminalImageCellClip,
        out: &mut MutationLog,
    ) -> MutationResult {
        let doomed: Vec<PlacementKey> = self
            .placements
            .iter()
            .filter(|(key, entry)| {
                key.0 == screen
                    && entry.placement.kind == TerminalImagePlacementKind::Sixel
                    && entry.placement.intersects_cells(area)
            })
            .map(|(key, _)| *key)
            .collect();
        for key in doomed {
            self.remove_placement(key, PlacementRemoval::GridEffect, out)?;
        }
        Ok(())
    }

    /// Scroll one screen's placements through a half-open margin.
    pub fn scroll(
        &mut self,
        screen: TerminalScreenKind,
        margin: TerminalImageCellClip,
        rows: i32,
        out: &mut MutationLog,
    ) -> MutationResult {
        self.retain_on_screen(screen, out, |placement| placement.apply_scroll(margin, rows))
    }

    /// Clip one screen's placements to a half-open viewport rectangle.
    pub fn clip_to_viewport(
        &mut self,
        screen: TerminalScreenKind,
        viewport: TerminalImageCellClip,
        out: &mut MutationLog,
    ) -> MutationResult {
        self.retain_on_screen(screen, out, |placement| placement.clip_to_viewport(viewport))
    }

    fn retain_on_screen(
        &mut self,
        screen: TerminalScreenKind,
        out: &mut MutationLog,
        mut keep: impl FnMut(&mut TerminalImagePlacement) -> bool,
    ) -> MutationResult {
        let mut doomed: Vec<PlacementKey> = Vec::new();
        let mut moved: Vec<TerminalImagePlacement> = Vec::new();
        for (key, entry) in &mut self.placements {
            if key.0 != screen {
                continue;
            }
            // Only `anchor.row` and `cell_clip` can move under a grid effect,
            // so republication stays limited to placements that really moved.
            let before = (entry.placement.anchor.row, entry.placement.cell_clip);
            if !keep(&mut entry.placement) {
                doomed.push(*key);
                continue;
            }
            if before != (entry.placement.anchor.row, entry.placement.cell_clip) {
                moved.push(entry.placement.clone());
            }
        }
        for placement in moved {
            out.push(CanonicalImageMutation::Place { screen, placement })?;
        }
        for key in doomed {
            self.remove_placement(key, PlacementRemoval::GridEffect, out)?;
        }
        Ok(())
    }

    /// Free the oldest definitions until one more fits the session ceiling.
    fn evict_definitions_for_one_more(&mut self, out: &mut MutationLog) -> MutationResult {
        let ceiling =
            usize::try_from(self.limits.max_images_per_session).unwrap_or(usize::MAX).max(1);
        while self.definitions.len() >= ceiling {
            let Some(image_id) = self.oldest_definition() else { break };
            let doomed: Vec<PlacementKey> =
                self.placements.keys().copied().filter(|key| key.1 == image_id).collect();
            for key in doomed {
                self.remove_placement(key, PlacementRemoval::Evicted, out)?;
            }
            self.definitions.remove(&image_id);
            out.push(CanonicalImageMutation::FreeImage { image_id, evicted: true })?;
        }
        Ok(())
    }

    /// Free the oldest placements until one more fits the session ceiling.
    fn evict_placements_for_one_more(&mut self, out: &mut MutationLog) -> MutationResult {
        let ceiling =
            usize::try_from(self.limits.max_placements_per_session).unwrap_or(usize::MAX).max(1);
        while self.placements.len() >= ceiling {
            let Some(key) = self.oldest_placement() else { break };
            self.remove_placement(key, PlacementRemoval::Evicted, out)?;
        }
        Ok(())
    }

    fn oldest_definition(&self) -> Option<TerminalImageId> {
        self.definitions
            .iter()
            .min_by_key(|(id, entry)| (entry.defined_at, **id))
            .map(|(id, _)| *id)
    }

    fn oldest_placement(&self) -> Option<PlacementKey> {
        self.placements
            .iter()
            .min_by_key(|(key, entry)| (entry.placed_at, **key))
            .map(|(key, _)| *key)
    }
}

/// Validate one placement's own scalars and its source rectangle against the
/// definition it references.
fn validated(
    placement: TerminalImagePlacement,
    definition: &TerminalImageDefinition,
) -> Result<TerminalImagePlacement, TerminalImageRejectionReason> {
    let within = placement
        .source
        .x
        .checked_add(placement.source.width)
        .is_some_and(|right| right <= definition.width)
        && placement
            .source
            .y
            .checked_add(placement.source.height)
            .is_some_and(|bottom| bottom <= definition.height);
    if placement.validate_scalars().is_err() || !within {
        return Err(TerminalImageRejectionReason::InvalidDimensions);
    }
    Ok(placement)
}

/// Conservative cell extent covering `width` by `height` source pixels.
fn cell_extent(width: u32, height: u32, context: MutationContext) -> Option<CellExtent> {
    let columns = width.div_ceil(u32::from(context.cell_width_pixels.max(1)));
    let rows = height.div_ceil(u32::from(context.cell_height_pixels.max(1)));
    Some(CellExtent {
        columns: u16::try_from(columns).ok().filter(|value| *value > 0)?,
        rows: u16::try_from(rows).ok().filter(|value| *value > 0)?,
    })
}

/// Build one canonical placement from a Kitty display command.
fn kitty_placement(
    command: &KittyCommand,
    definition: &TerminalImageDefinition,
    generation: TerminalImageGeneration,
    context: MutationContext,
) -> Result<TerminalImagePlacement, TerminalImageRejectionReason> {
    let invalid = TerminalImageRejectionReason::InvalidDimensions;
    let x = command.source_x.unwrap_or(0);
    let y = command.source_y.unwrap_or(0);
    // An oversized `w=`/`h=` is a protocol error, not something to clamp.
    let width = command.source_width.unwrap_or_else(|| definition.width.saturating_sub(x));
    let height = command.source_height.unwrap_or_else(|| definition.height.saturating_sub(y));
    if width == 0 || height == 0 {
        return Err(invalid);
    }
    let source = PixelRect { x, y, width, height };
    let derived = cell_extent(width, height, context).ok_or(invalid)?;
    let destination = CellExtent {
        columns: command
            .columns
            .map_or(Ok(derived.columns), |value| u16::try_from(value).map_err(|_| invalid))?,
        rows: command
            .rows
            .map_or(Ok(derived.rows), |value| u16::try_from(value).map_err(|_| invalid))?,
    };
    let placeholder = match command.placement_mode {
        KittyPlacementMode::Classic => None,
        KittyPlacementMode::UnicodePlaceholder => Some(PlaceholderMetadata {
            // Kitty encodes 24 identifier bits in the foreground colour and
            // adds the most significant byte only when the image needs it.
            image_identity_bits: if definition.id.0 > 0x00ff_ffff { 32 } else { 24 },
            placement_id_in_underline: command.placement_id.is_some(),
            background_alpha: 0,
        }),
    };
    let kind = match command.placement_mode {
        KittyPlacementMode::Classic => TerminalImagePlacementKind::KittyClassic,
        KittyPlacementMode::UnicodePlaceholder => {
            TerminalImagePlacementKind::KittyUnicodePlaceholder
        }
    };
    Ok(TerminalImagePlacement {
        id: TerminalPlacementId(u64::from(command.placement_id.unwrap_or(0))),
        image_id: definition.id,
        generation,
        protocol: TerminalImageProtocol::Kitty,
        kind,
        anchor: TerminalCellAnchor { row: context.cursor_row, column: context.cursor_column },
        source,
        destination,
        pixel_offset_x: u16::try_from(command.pixel_x.unwrap_or(0)).map_err(|_| invalid)?,
        pixel_offset_y: u16::try_from(command.pixel_y.unwrap_or(0)).map_err(|_| invalid)?,
        z_index: command.z_index.unwrap_or(0),
        scrolls_with_grid: true,
        // Kitty's `C=1` suppresses cursor movement; `C=0` and an omitted `C`
        // both leave the cursor after a classic image.
        move_cursor: command.placement_mode == KittyPlacementMode::Classic
            && !command.move_cursor.unwrap_or(false),
        cell_clip: None,
        placeholder,
    })
}

/// Translate one Kitty `a=d` command into an exact canonical delete.
///
/// Every operand keeps its own presence: an omitted `i=`, `p=`, `x=`, `y=`, or
/// `z=` stays `None` and matches everything in scope, while an explicit `0`
/// stays a literal value that matches only identity or coordinate zero.
#[must_use]
pub fn kitty_delete(command: &KittyCommand) -> TerminalImageDelete {
    // Kitty defaults `d` to `a` only when the operand is omitted entirely.
    let selector = command.delete.map_or('a', |delete| delete.selector);
    let free_image_data = command.delete.is_some_and(|delete| delete.free_data);
    let scope = match selector.to_ascii_lowercase() {
        'i' if command.placement_id.is_some() => TerminalImageDeleteScope::Placement,
        'i' => TerminalImageDeleteScope::Image,
        'p' => TerminalImageDeleteScope::Cell,
        'x' => TerminalImageDeleteScope::Column,
        'y' => TerminalImageDeleteScope::Row,
        'z' => TerminalImageDeleteScope::ZIndex,
        _ => TerminalImageDeleteScope::AllPlacements,
    };
    // Kitty cell operands are 1-based; canonical anchors are 0-based.
    let cell = |value: Option<u32>| {
        value.map(|value| i32::try_from(value.saturating_sub(1)).unwrap_or(i32::MAX))
    };
    let coordinate = match scope {
        TerminalImageDeleteScope::Cell | TerminalImageDeleteScope::Column => cell(command.source_x),
        TerminalImageDeleteScope::Row => cell(command.source_y),
        TerminalImageDeleteScope::ZIndex => command.z_index,
        _ => None,
    };
    TerminalImageDelete {
        scope,
        image_id: command.image_id.map(|id| TerminalImageId(u64::from(id))),
        placement_id: command.placement_id.map(|id| TerminalPlacementId(u64::from(id))),
        coordinate,
        free_image_data,
    }
}
