//! Production-path evidence for transactional terminal-image mutations.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{
    TerminalImageDefinition, TerminalImagePlacement, TerminalImagePlacementKind,
    TerminalImageProtocol, TerminalImageRejectionReason, TerminalScreenKind,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_pty::graphics_framing::GraphicsStorageRejection;
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_mutations::{
    CanonicalImageMutation, MutationLog, PlacementRemoval,
};
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, StorageAllocationClass, TerminalImageProcessPolicy,
    feed_terminal_image_result_observed, observe_terminal_resize,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

/// Cell metrics used by every case so derived cell extents stay predictable.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

/// One named transactional case and the closed check that proves it.
type NamedCase = (&'static str, fn() -> Result<(), String>);

/// One black RGB pixel: the smallest definition the Kitty decoder accepts.
const ONE_PIXEL_RGB: &[u8] = &[0, 0, 0];

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    limits: LimitsEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

#[derive(Serialize)]
struct LimitsEvidence {
    max_images_per_session: u32,
    max_placements_per_session: u32,
}

struct ProbeDimensions {
    columns: usize,
    rows: usize,
}

impl Dimensions for ProbeDimensions {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Production seam plus the real Alacritty terminal that observes it.
struct Probe {
    images: PtyTerminalImageState,
    term: Term<ScribeEventListener>,
    processor: Processor,
    _event_rx: mpsc::UnboundedReceiver<SessionEvent>,
}

impl Probe {
    fn new() -> Self {
        Self::with_policy(TerminalImageProcessPolicy::v1())
    }

    fn with_policy(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(SessionId::new(), event_tx);
        let dimensions = ProbeDimensions { columns: 40, rows: 10 };
        let term = Term::new(build_term_config(32), &dimensions, listener);
        let images = PtyTerminalImageState::new(policy);
        images.grid_observer().set_cell_size(CELL_WIDTH, CELL_HEIGHT);
        Self { images, term, processor: Processor::new(), _event_rx: event_rx }
    }

    /// Drive one PTY read through framing, the real terminal, and the
    /// transactional mutation commit, in production order.
    fn feed(&mut self, bytes: &[u8]) -> Result<MutationLog, String> {
        let mut result = self.images.process_bytes(bytes);
        feed_terminal_image_result_observed(
            &mut self.images,
            &mut self.term,
            &mut self.processor,
            bytes,
            &mut result,
        );
        let commit = result.map_err(|error| error.to_string())?;
        self.images.commit_mutations(&commit).map_err(|error| error.to_string())
    }

    fn resize(&mut self, columns: usize, rows: usize) -> Result<MutationLog, String> {
        let before = (self.term.columns(), self.term.screen_lines());
        self.term.resize(ProbeDimensions { columns, rows });
        let changed = before != (self.term.columns(), self.term.screen_lines());
        let observer = self.images.grid_observer();
        let span = observe_terminal_resize(&observer, &self.term, changed);
        self.images.record_grid_observation(&span.observation);
        self.images.commit_span_mutations(&span).map_err(|error| error.to_string())
    }

    fn definitions(&self) -> Vec<TerminalImageDefinition> {
        self.images.canonical_definitions()
    }

    fn placements(&self) -> Vec<(TerminalScreenKind, TerminalImagePlacement)> {
        self.images.canonical_placements()
    }

    fn image_ids(&self) -> Vec<u64> {
        self.definitions().iter().map(|definition| definition.id.0).collect()
    }

    fn placement_keys(&self) -> Vec<(TerminalScreenKind, u64, u64)> {
        self.placements()
            .iter()
            .map(|(screen, placement)| (*screen, placement.image_id.0, placement.id.0))
            .collect()
    }
}

/// Encode one Kitty APC command with a base64 direct payload.
fn kitty(controls: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{controls};{}\x1b\\", STANDARD.encode(payload)).into_bytes()
}

/// Transmit-and-display one 1x1 RGB image under an explicit identifier.
fn transmit_display(image_id: u32) -> Vec<u8> {
    kitty(&format!("a=T,f=24,s=1,v=1,i={image_id}"), ONE_PIXEL_RGB)
}

fn control(bytes: &str) -> Vec<u8> {
    bytes.as_bytes().to_vec()
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

// @lat: [[test#Test Harness#Transactional Image Mutations#Production Mutation Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();
    let checks: [NamedCase; 12] = [
        ("atomic_define_and_place", case_atomic_define_and_place),
        ("compound_failure_commits_nothing", case_compound_failure_commits_nothing),
        ("rollback_preserves_prior_state", case_rollback_preserves_prior_state),
        ("exact_delete_identity", case_exact_delete_identity),
        ("omitted_operand_is_not_wildcard", case_omitted_operand_is_not_wildcard),
        ("deterministic_image_eviction", case_deterministic_image_eviction),
        ("deterministic_placement_eviction", case_deterministic_placement_eviction),
        ("screen_scoped_mutations", case_screen_scoped_mutations),
        ("kitty_lifecycle_erases", case_kitty_lifecycle_erases),
        ("kitty_immune_to_text_erase", case_kitty_immune_to_text_erase),
        ("half_open_area_and_scroll", case_half_open_area_and_scroll),
        ("resize_clips_both_grids", case_resize_clips_both_grids),
    ];
    for (name, check) in checks {
        check().map_err(|error| format!("{name}: {error}"))?;
        cases.insert(name, "pass");
    }
    let limits = TerminalImageProcessPolicy::v1().limits();
    write_evidence(
        evidence_path,
        &Evidence {
            schema_version: 1,
            status: "pass",
            engine: "scribe-server canonical image mutations",
            payload_free: true,
            limits: LimitsEvidence {
                max_images_per_session: limits.max_images_per_session,
                max_placements_per_session: limits.max_placements_per_session,
            },
            cases,
        },
    )
}

/// A transmit-and-display publishes its definition and placement together and
/// anchors the placement at the cursor the final chunk observed.
fn case_atomic_define_and_place() -> Result<(), String> {
    let mut probe = Probe::new();
    let log = probe.feed(&concat(&[control("\x1b[3;5H"), transmit_display(7)]))?;
    let mutations = log.as_slice();
    match mutations {
        [
            CanonicalImageMutation::Define { definition },
            CanonicalImageMutation::Place { screen, placement },
        ] => {
            if definition.id.0 != 7 || definition.width != 1 || definition.height != 1 {
                return Err(format!("unexpected definition {definition:?}"));
            }
            if *screen != TerminalScreenKind::Primary
                || placement.anchor.row != 2
                || placement.anchor.column != 4
                || placement.protocol != TerminalImageProtocol::Kitty
                || placement.kind != TerminalImagePlacementKind::KittyClassic
            {
                return Err(format!("unexpected placement {placement:?}"));
            }
        }
        other => return Err(format!("expected one define and one place, got {other:?}")),
    }
    if probe.image_ids() != vec![7] || probe.placement_keys().len() != 1 {
        return Err("canonical state did not retain the compound mutation".to_owned());
    }
    Ok(())
}

/// An invalid display operand rejects the whole compound command, so no
/// definition survives a placement that could never be published.
fn case_compound_failure_commits_nothing() -> Result<(), String> {
    let mut probe = Probe::new();
    // `w=` selects a source rectangle wider than the transmitted image.
    let log = probe.feed(&kitty("a=T,f=24,s=1,v=1,i=3,w=9", ONE_PIXEL_RGB))?;
    if !matches!(
        log.as_slice(),
        [CanonicalImageMutation::Reject {
            reason: TerminalImageRejectionReason::InvalidDimensions
        }]
    ) {
        return Err(format!("expected one typed rejection, got {:?}", log.as_slice()));
    }
    if !probe.definitions().is_empty() || !probe.placements().is_empty() {
        return Err("rejected compound command left canonical state behind".to_owned());
    }

    // Placing an image that was never transmitted is a typed miss, not a panic.
    let missing = probe.feed(&kitty("a=p,i=404", &[]))?;
    if !matches!(
        missing.as_slice(),
        [CanonicalImageMutation::Reject { reason: TerminalImageRejectionReason::ImageNotFound }]
    ) {
        return Err(format!("expected an image-not-found rejection, got {:?}", missing.as_slice()));
    }
    Ok(())
}

/// A storage rejection inside the mutation phase restores the prior canonical
/// state, its counters, and the session's retained ownership.
fn case_rollback_preserves_prior_state() -> Result<(), String> {
    // Calibrate how many mutation-class reservations one successful read makes
    // without ever firing, then aim the fault at the next read's first one.
    let mut calibration =
        Probe::with_policy(TerminalImageProcessPolicy::with_storage_rejection_for_validation(
            u64::MAX,
            u64::MAX,
            StorageAllocationClass::CanonicalMutations,
            u64::MAX,
            GraphicsStorageRejection::SessionLimit,
        ));
    calibration.feed(&transmit_display(1))?;
    let (reservations, fired, _) = calibration.images.validation_rejection_observation();
    if fired != 0 {
        return Err("calibration run fired the injected rejection".to_owned());
    }

    let mut probe =
        Probe::with_policy(TerminalImageProcessPolicy::with_storage_rejection_for_validation(
            u64::MAX,
            u64::MAX,
            StorageAllocationClass::CanonicalMutations,
            reservations.saturating_add(1),
            GraphicsStorageRejection::SessionLimit,
        ));
    probe.feed(&transmit_display(1))?;
    let definitions = probe.definitions();
    let placements = probe.placement_keys();
    let ownership = probe.images.storage_ownership();

    let Err(error) = probe.feed(&transmit_display(2)) else {
        return Err("injected mutation-storage rejection did not fail the read".to_owned());
    };
    if !error.contains("storage") {
        return Err(format!("unexpected rollback error: {error}"));
    }
    if probe.definitions() != definitions || probe.placement_keys() != placements {
        return Err("rolled-back read changed canonical state".to_owned());
    }
    if probe.images.storage_ownership() != ownership {
        return Err("rolled-back read leaked storage ownership".to_owned());
    }
    Ok(())
}

/// Delete selectors address exactly the protocol identity they name, and the
/// uppercase polarity is what frees canonical image data.
fn case_exact_delete_identity() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[transmit_display(1), transmit_display(2)]))?;
    if probe.image_ids() != vec![1, 2] {
        return Err("setup did not define both images".to_owned());
    }

    // Lowercase `d=i` removes placements for image 1 and keeps its data.
    let log = probe.feed(&kitty("a=d,d=i,i=1", &[]))?;
    if !log.as_slice().iter().any(|mutation| {
        matches!(
            mutation,
            CanonicalImageMutation::RemovePlacement {
                image_id, reason: PlacementRemoval::Deleted, ..
            } if image_id.0 == 1
        )
    }) {
        return Err(format!("delete did not target image 1: {:?}", log.as_slice()));
    }
    if probe.image_ids() != vec![1, 2] {
        return Err("soft delete freed image data".to_owned());
    }
    if probe.placement_keys().iter().any(|(_, image_id, _)| *image_id == 1) {
        return Err("soft delete left image 1 placed".to_owned());
    }

    // Uppercase `d=I` also frees the unreferenced definition.
    probe.feed(&kitty("a=d,d=I,i=2", &[]))?;
    if probe.image_ids() != vec![1] {
        return Err(format!("hard delete did not free image 2: {:?}", probe.image_ids()));
    }
    Ok(())
}

/// An omitted operand cannot masquerade as an explicit zero: `d=z` with no `z`
/// matches nothing, while `z=0` matches only the zero-index placement.
fn case_omitted_operand_is_not_wildcard() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[
        kitty("a=T,f=24,s=1,v=1,i=1,z=0", ONE_PIXEL_RGB),
        kitty("a=T,f=24,s=1,v=1,i=2,z=5", ONE_PIXEL_RGB),
    ]))?;
    if probe.placement_keys().len() != 2 {
        return Err("setup did not place both z-index images".to_owned());
    }

    probe.feed(&kitty("a=d,d=z", &[]))?;
    if probe.placement_keys().len() != 2 {
        return Err("omitted z operand behaved as a wildcard".to_owned());
    }

    probe.feed(&kitty("a=d,d=z,z=0", &[]))?;
    let remaining = probe.placement_keys();
    if remaining.len() != 1 || remaining.first().map(|key| key.1) != Some(2) {
        return Err(format!("explicit z=0 deleted the wrong placements: {remaining:?}"));
    }
    Ok(())
}

/// Exceeding the session image ceiling evicts the oldest definition first and
/// publishes its removal before the definition that displaced it.
fn case_deterministic_image_eviction() -> Result<(), String> {
    let mut probe = Probe::new();
    let ceiling = TerminalImageProcessPolicy::v1().limits().max_images_per_session;
    for image_id in 1..=ceiling {
        probe.feed(&transmit_display(image_id))?;
    }
    if probe.definitions().len() != ceiling as usize {
        return Err("setup did not fill the session image ceiling".to_owned());
    }

    let log = probe.feed(&transmit_display(ceiling + 1))?;
    let mutations = log.as_slice();
    let evicted = mutations.iter().position(|mutation| {
        matches!(mutation, CanonicalImageMutation::FreeImage { image_id, evicted: true }
            if image_id.0 == 1)
    });
    let defined = mutations
        .iter()
        .position(|mutation| matches!(mutation, CanonicalImageMutation::Define { .. }));
    match (evicted, defined) {
        (Some(evicted), Some(defined)) if evicted < defined => {}
        _ => return Err(format!("eviction did not precede the new define: {mutations:?}")),
    }
    let ids = probe.image_ids();
    if ids.len() != ceiling as usize || ids.contains(&1) || !ids.contains(&u64::from(ceiling + 1)) {
        return Err(format!("eviction order was not oldest-first: {ids:?}"));
    }
    Ok(())
}

/// Exceeding the session placement ceiling evicts the oldest placement while
/// leaving every other exact identity intact.
fn case_deterministic_placement_eviction() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    let ceiling = TerminalImageProcessPolicy::v1().limits().max_placements_per_session;
    // The transmit-and-display above already owns placement id 0.
    let mut pending: Vec<Vec<u8>> = Vec::new();
    for placement_id in 1..=ceiling {
        pending.push(kitty(&format!("a=p,i=1,p={placement_id}"), &[]));
        if pending.len() == 64 {
            probe.feed(&concat(&pending))?;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        probe.feed(&concat(&pending))?;
    }
    let keys = probe.placement_keys();
    if keys.len() != ceiling as usize {
        return Err(format!("expected {ceiling} placements, found {}", keys.len()));
    }
    if keys.iter().any(|key| key.2 == 0) {
        return Err("oldest placement survived the placement ceiling".to_owned());
    }
    if !keys.iter().any(|key| key.2 == u64::from(ceiling)) {
        return Err("newest placement was not committed".to_owned());
    }
    Ok(())
}

/// Placements belong to the screen that was active when they were created, and
/// entering the alternate screen creates a grid with no images on it.
fn case_screen_scoped_mutations() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    probe.feed(&concat(&[control("\x1b[?1049h"), transmit_display(2)]))?;
    let keys = probe.placement_keys();
    if keys.iter().filter(|key| key.0 == TerminalScreenKind::Primary).count() != 1
        || keys.iter().filter(|key| key.0 == TerminalScreenKind::Alternate).count() != 1
    {
        return Err(format!("placements were not screen scoped: {keys:?}"));
    }

    probe.feed(&control("\x1b[?1049l"))?;
    if probe.placement_keys() != keys {
        return Err("leaving the alternate screen changed placements".to_owned());
    }

    probe.feed(&control("\x1b[?1049h"))?;
    let reentered = probe.placement_keys();
    if reentered.iter().any(|key| key.0 == TerminalScreenKind::Alternate) {
        return Err(format!("alternate creation kept stale placements: {reentered:?}"));
    }
    if reentered.iter().filter(|key| key.0 == TerminalScreenKind::Primary).count() != 1 {
        return Err("alternate creation disturbed the primary screen".to_owned());
    }
    Ok(())
}

/// ED2 clears every visible placement on its own screen and a hard reset drops
/// definitions as well, matching the Kitty image lifecycle.
fn case_kitty_lifecycle_erases() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    probe.feed(&control("\x1b[2J"))?;
    if !probe.placements().is_empty() {
        return Err("ED2 left visible placements".to_owned());
    }
    if probe.image_ids() != vec![1] {
        return Err("ED2 discarded canonical image data".to_owned());
    }

    probe.feed(&transmit_display(2))?;
    probe.feed(&control("\x1bc"))?;
    if !probe.definitions().is_empty() || !probe.placements().is_empty() {
        return Err("hard reset left canonical image state".to_owned());
    }
    Ok(())
}

/// Ordinary text erases move terminal content, not Kitty graphics; only Sixel
/// placements share the erased cells.
fn case_kitty_immune_to_text_erase() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[control("\x1b[1;1H"), transmit_display(1), control("\x1bPq~\x1b\\")]))?;
    let before = probe.placement_keys();
    if before.len() != 2 {
        return Err(format!("setup did not place both protocols: {before:?}"));
    }

    // Erase the whole first line: the Sixel placement shares those cells.
    probe.feed(&control("\x1b[1;1H\x1b[2K"))?;
    let remaining = probe.placements();
    if remaining.len() != 1
        || remaining.first().map(|(_, placement)| placement.protocol)
            != Some(TerminalImageProtocol::Kitty)
    {
        return Err(format!("text erase did not spare Kitty graphics: {remaining:?}"));
    }
    Ok(())
}

/// Erase rectangles and scroll margins are half-open on both edges, so a cell
/// exactly on the exclusive bound is untouched.
fn case_half_open_area_and_scroll() -> Result<(), String> {
    let mut probe = Probe::new();
    // Two Sixel images: one in column 0 of row 0, one in column 1.
    probe.feed(&concat(&[
        control("\x1b[1;1H"),
        control("\x1bPq~\x1b\\"),
        control("\x1b[1;2H"),
        control("\x1bPq~\x1b\\"),
    ]))?;
    if probe.placements().len() != 2 {
        return Err("setup did not place both Sixel images".to_owned());
    }
    // Erase exactly one cell at row 0, column 0: [0,1) x [0,1).
    probe.feed(&control("\x1b[1;1H\x1b[1X"))?;
    let remaining = probe.placements();
    if remaining.len() != 1
        || remaining.first().map(|(_, placement)| placement.anchor.column) != Some(1)
    {
        return Err(format!("erase bound was not half-open: {remaining:?}"));
    }

    // Two Kitty images: rows 5 and 6 of a [2,6) scroll region.
    let mut scrolled = Probe::new();
    scrolled.feed(&concat(&[
        control("\x1b[6;1H"),
        transmit_display(1),
        control("\x1b[7;1H"),
        transmit_display(2),
    ]))?;
    scrolled.feed(&control("\x1b[3;6r\x1b[1S"))?;
    let rows: BTreeMap<u64, i32> = scrolled
        .placements()
        .iter()
        .map(|(_, placement)| (placement.image_id.0, placement.anchor.row))
        .collect();
    if rows.get(&1) != Some(&4) {
        return Err(format!("in-margin placement did not scroll: {rows:?}"));
    }
    if rows.get(&2) != Some(&6) {
        return Err(format!("placement on the exclusive margin scrolled: {rows:?}"));
    }
    Ok(())
}

/// One resize clips placements on the active and inactive grids alike, so a
/// later screen switch cannot resurrect out-of-bounds images.
fn case_resize_clips_both_grids() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[control("\x1b[8;1H"), transmit_display(1)]))?;
    probe.feed(&concat(&[control("\x1b[?1049h\x1b[8;1H"), transmit_display(2)]))?;
    probe.feed(&control("\x1b[?1049l"))?;
    if probe.placements().len() != 2 {
        return Err("setup did not place one image per screen".to_owned());
    }

    probe.resize(40, 4)?;
    let remaining = probe.placement_keys();
    if !remaining.is_empty() {
        return Err(format!("resize left out-of-bounds placements: {remaining:?}"));
    }
    Ok(())
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence<'_>) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode mutation evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
