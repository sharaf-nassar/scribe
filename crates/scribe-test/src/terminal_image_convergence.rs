//! Production-path evidence for client convergence and counter safety.
//!
//! Every case drives real PTY bytes through production framing, the real
//! Alacritty terminal, and the server's transactional mutation commit, then
//! replays the records the server published onto the production client scene
//! and compares both canonical models.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use scribe_client::terminal_image_scene::{CommittedImageScene, LiveImageScene, LiveSceneError};
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{
    TerminalImageDefinition, TerminalImageLiveMessage, TerminalImagePlacement, TerminalScreenKind,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, SessionTerminalError, TerminalImageProcessPolicy,
    feed_terminal_image_result_observed, observe_terminal_resize,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

/// Cell metrics used by every case so derived cell extents stay predictable.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

/// One named convergence case and the closed check that proves it.
type NamedCase = (&'static str, fn() -> Result<(), String>);

/// One black RGB pixel: the smallest definition the Kitty decoder accepts.
const ONE_PIXEL_RGB: &[u8] = &[0, 0, 0];

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    cases: BTreeMap<&'a str, &'a str>,
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

/// Server seam, the real terminal that observes it, and the production client
/// scene the seam publishes to.
struct Probe {
    images: PtyTerminalImageState,
    term: Term<ScribeEventListener>,
    processor: Processor,
    scene: LiveImageScene,
    last_burst: Vec<TerminalImageLiveMessage>,
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
        Self {
            images,
            term,
            processor: Processor::new(),
            scene: LiveImageScene::default(),
            last_burst: Vec::new(),
            _event_rx: event_rx,
        }
    }

    /// Drive one PTY read through framing, the real terminal, the canonical
    /// mutation commit, and publication, then apply the burst to the client.
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut result = self.images.process_bytes(bytes);
        feed_terminal_image_result_observed(
            &mut self.images,
            &mut self.term,
            &mut self.processor,
            bytes,
            &mut result,
        );
        let commit = result.map_err(|error| error.to_string())?;
        let messages = self
            .images
            .commit_and_publish(&commit, &mut definition_payload)
            .map_err(|error| error.to_string())?;
        self.apply(messages)
    }

    /// Drive one out-of-band resize span through the same publication path.
    fn resize(&mut self, columns: usize, rows: usize) -> Result<(), String> {
        let before = (self.term.columns(), self.term.screen_lines());
        self.term.resize(ProbeDimensions { columns, rows });
        let changed = before != (self.term.columns(), self.term.screen_lines());
        let observer = self.images.grid_observer();
        let span = observe_terminal_resize(&observer, &self.term, changed);
        self.images.record_grid_observation(&span.observation);
        let messages = self
            .images
            .commit_span_and_publish(&span, &mut definition_payload)
            .map_err(|error| error.to_string())?;
        self.apply(messages)
    }

    fn apply(&mut self, messages: Vec<TerminalImageLiveMessage>) -> Result<(), String> {
        for message in &messages {
            self.scene.apply(message.clone()).map_err(|error| format!("client apply: {error}"))?;
        }
        if !messages.is_empty() {
            self.last_burst = messages;
        }
        Ok(())
    }

    fn committed(&self) -> Arc<CommittedImageScene> {
        self.scene.committed()
    }

    /// Compare the server's canonical model with the client's committed scene.
    fn converged(&self) -> Result<(), String> {
        let scene = self.committed();
        let server_definitions = sorted_definitions(self.images.canonical_definitions());
        let client_definitions = sorted_definitions(
            scene.definitions.iter().map(|entry| entry.metadata.clone()).collect(),
        );
        if server_definitions != client_definitions {
            return Err(format!(
                "definitions diverged: server {server_definitions:?} client {client_definitions:?}"
            ));
        }

        let server_placements = sorted_placements(self.images.canonical_placements());
        let mut client_pairs: Vec<(TerminalScreenKind, TerminalImagePlacement)> = Vec::new();
        client_pairs.extend(
            scene
                .primary_placements
                .iter()
                .map(|placement| (TerminalScreenKind::Primary, placement.clone())),
        );
        client_pairs.extend(
            scene
                .alternate_placements
                .iter()
                .map(|placement| (TerminalScreenKind::Alternate, placement.clone())),
        );
        let client_placements = sorted_placements(client_pairs);
        if server_placements != client_placements {
            return Err(format!(
                "placements diverged: server {server_placements:?} client {client_placements:?}"
            ));
        }

        let state = self.images.state();
        if scene.active_screen != state.active_screen {
            return Err(format!(
                "active screen diverged: server {:?} client {:?}",
                state.active_screen, scene.active_screen
            ));
        }
        if !server_definitions.is_empty() || !server_placements.is_empty() {
            // A published scene must carry the generation its records were
            // committed under; an empty scene may predate any publication.
            if scene.generation != Some(state.generation) {
                return Err(format!(
                    "generation diverged: server {:?} client {:?}",
                    state.generation, scene.generation
                ));
            }
        }
        Ok(())
    }
}

/// Canonical bytes for one published definition.
///
/// The server seam is payload-free by design, so the caller that owns decoded
/// pixels supplies them. Deterministic filler keeps this gate about ordering
/// and convergence rather than decoder output.
fn definition_payload(definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
    let length = usize::try_from(definition.rgba_bytes).ok()?;
    Some(vec![u8::try_from(definition.id.0 % 251).unwrap_or(0); length])
}

fn sorted_definitions(
    mut definitions: Vec<TerminalImageDefinition>,
) -> Vec<TerminalImageDefinition> {
    definitions.sort_by_key(|definition| definition.id.0);
    definitions
}

fn sorted_placements(
    mut placements: Vec<(TerminalScreenKind, TerminalImagePlacement)>,
) -> Vec<(TerminalScreenKind, TerminalImagePlacement)> {
    placements.sort_by_key(|(screen, placement)| {
        (u8::from(*screen == TerminalScreenKind::Alternate), placement.image_id.0, placement.id.0)
    });
    placements
}

/// Encode one Kitty APC command with a base64 direct payload.
pub fn kitty(controls: &str, payload: &[u8]) -> Vec<u8> {
    format!("\x1b_G{controls};{}\x1b\\", STANDARD.encode(payload)).into_bytes()
}

/// Transmit-and-display one 1x1 RGB image under an explicit identifier.
pub fn transmit_display(image_id: u32) -> Vec<u8> {
    kitty(&format!("a=T,f=24,s=1,v=1,i={image_id}"), ONE_PIXEL_RGB)
}

pub fn control(bytes: &str) -> Vec<u8> {
    bytes.as_bytes().to_vec()
}

pub fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

// @lat: [[test#Test Harness#Client Convergence and Counter Safety#Production Convergence Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();
    let checks: [NamedCase; 8] = [
        ("definitions_and_placements_converge", case_definitions_and_placements),
        ("removals_converge", case_removals),
        ("reset_converges", case_reset),
        ("screen_change_converges", case_screen_change),
        ("scroll_converges", case_scroll),
        ("resize_converges", case_resize),
        ("stale_replay_rejected", case_stale_replay),
        ("counter_exhaustion_rejects_before_mutation", case_counter_exhaustion),
    ];
    for (name, check) in checks {
        check().map_err(|error| format!("{name}: {error}"))?;
        cases.insert(name, "pass");
    }
    write_evidence(
        evidence_path,
        &Evidence {
            schema_version: 1,
            status: "pass",
            engine: "scribe-server canonical image publication",
            payload_free: true,
            cases,
        },
    )
}

/// Definitions, replacement, and additional placements converge to the same
/// canonical model on both sides of the publication boundary.
fn case_definitions_and_placements() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[control("\x1b[3;5H"), transmit_display(7)]))?;
    probe.converged()?;
    if probe.committed().placements().len() != 1 {
        return Err("client did not receive the compound define and place".to_owned());
    }

    // A second placement of the same image and a second image.
    probe.feed(&concat(&[control("\x1b[5;2H"), kitty("a=p,i=7,p=3", &[])]))?;
    probe.feed(&concat(&[control("\x1b[7;1H"), transmit_display(8)]))?;
    probe.converged()?;
    if probe.images.canonical_placements().len() != 3 {
        return Err("server did not retain three placements".to_owned());
    }

    // Retransmitting image 7 replaces its data; both sides keep the same
    // surviving placements afterwards.
    probe.feed(&transmit_display(7))?;
    probe.converged()
}

/// Soft and hard Kitty deletes converge, including the freed definition.
fn case_removals() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[transmit_display(1), transmit_display(2)]))?;
    probe.converged()?;

    probe.feed(&kitty("a=d,d=i,i=1", &[]))?;
    probe.converged()?;
    if probe.committed().definitions.len() != 2 {
        return Err("soft delete freed client image data".to_owned());
    }

    probe.feed(&kitty("a=d,d=I,i=2", &[]))?;
    probe.converged()?;
    if probe.committed().definitions.len() != 1 {
        return Err("hard delete did not free the client definition".to_owned());
    }
    Ok(())
}

/// ED2 and a hard reset converge, and the reset opens the next generation on
/// both sides without leaving stale client state behind.
fn case_reset() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    let before = probe.images.state().generation;

    probe.feed(&control("\x1b[2J"))?;
    probe.converged()?;
    if !probe.committed().placements().is_empty() {
        return Err("ED2 left client placements".to_owned());
    }

    probe.feed(&control("\x1bc"))?;
    probe.converged()?;
    let after = probe.images.state().generation;
    if after.0 != before.0 + 1 {
        return Err(format!("reset did not open one new generation: {before:?} -> {after:?}"));
    }
    let scene = probe.committed();
    if !scene.definitions.is_empty() || !scene.primary_placements.is_empty() {
        return Err("hard reset left client image state".to_owned());
    }

    // The generation the client now holds must accept the next publication.
    probe.feed(&transmit_display(2))?;
    probe.converged()
}

/// Placements stay on the screen that owned them, and the client follows every
/// screen switch, including the fresh alternate grid.
fn case_screen_change() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    probe.feed(&concat(&[control("\x1b[?1049h"), transmit_display(2)]))?;
    probe.converged()?;
    let scene = probe.committed();
    if scene.active_screen != TerminalScreenKind::Alternate
        || scene.primary_placements.len() != 1
        || scene.alternate_placements.len() != 1
    {
        return Err(format!("client screen buckets diverged: {:?}", scene.placements()));
    }

    probe.feed(&control("\x1b[?1049l"))?;
    probe.converged()?;
    probe.feed(&control("\x1b[?1049h"))?;
    probe.converged()?;
    if !probe.committed().alternate_placements.is_empty() {
        return Err("alternate creation kept stale client placements".to_owned());
    }
    Ok(())
}

/// A scroll inside a half-open margin republishes exactly the placements that
/// moved, so both sides agree on every anchor row.
fn case_scroll() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[
        control("\x1b[6;1H"),
        transmit_display(1),
        control("\x1b[7;1H"),
        transmit_display(2),
    ]))?;
    probe.converged()?;

    probe.feed(&control("\x1b[3;6r\x1b[1S"))?;
    probe.converged()?;
    let rows: BTreeMap<u64, i32> = probe
        .committed()
        .placements()
        .iter()
        .map(|placement| (placement.image_id.0, placement.anchor.row))
        .collect();
    if rows.get(&1) != Some(&4) || rows.get(&2) != Some(&6) {
        return Err(format!("client scroll rows diverged: {rows:?}"));
    }
    Ok(())
}

/// One resize clips the active and inactive grids alike on both sides.
fn case_resize() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&concat(&[control("\x1b[8;1H"), transmit_display(1)]))?;
    probe.feed(&concat(&[control("\x1b[?1049h\x1b[8;1H"), transmit_display(2)]))?;
    probe.feed(&control("\x1b[?1049l"))?;
    probe.converged()?;

    probe.resize(40, 4)?;
    probe.converged()?;
    let scene = probe.committed();
    if !scene.primary_placements.is_empty() || !scene.alternate_placements.is_empty() {
        return Err("resize left out-of-bounds client placements".to_owned());
    }
    Ok(())
}

/// Replaying an already-applied burst is rejected as stale and leaves the
/// published scene byte-identical.
fn case_stale_replay() -> Result<(), String> {
    let mut probe = Probe::new();
    probe.feed(&transmit_display(1))?;
    let stale = probe.last_burst.clone();
    probe.feed(&concat(&[control("\x1b[4;1H"), transmit_display(2)]))?;
    let published = probe.committed();

    let mut rejected = 0usize;
    for message in stale {
        match probe.scene.apply(message) {
            Err(LiveSceneError::StaleGeneration | LiveSceneError::StaleSequence) => rejected += 1,
            // The stale boundary closed the burst, so its own records can no
            // longer reach staged state at all.
            Err(LiveSceneError::UpdateWithoutBegin | LiveSceneError::CommitWithoutBegin) => {}
            Err(error) => return Err(format!("unexpected replay error: {error}")),
            Ok(_) => return Err("stale burst was accepted".to_owned()),
        }
    }
    if rejected == 0 {
        return Err("replay produced no typed staleness rejection".to_owned());
    }
    if !Arc::ptr_eq(&published, &probe.committed()) {
        return Err("stale replay replaced the published scene".to_owned());
    }
    probe.converged()
}

/// Sequence and generation exhaustion both reject before any canonical
/// mutation or publication, leaving the last committed state intact.
fn case_counter_exhaustion() -> Result<(), String> {
    // One image boundary fits the ceiling; the publication that would follow
    // it does not.
    let mut sequence =
        Probe::with_policy(TerminalImageProcessPolicy::with_sequence_ceiling_for_validation(1));
    match sequence.feed(&transmit_display(1)) {
        Err(error) if error == SessionTerminalError::SequenceExhausted.to_string() => {}
        other => return Err(format!("expected a typed sequence rejection, got {other:?}")),
    }
    if sequence.images.state().sequence.0 != 1 {
        return Err("framing did not reach the publication preflight".to_owned());
    }
    if !sequence.images.canonical_definitions().is_empty()
        || !sequence.images.canonical_placements().is_empty()
    {
        return Err("sequence exhaustion mutated canonical state".to_owned());
    }
    if !sequence.committed().definitions.is_empty() {
        return Err("sequence exhaustion published a partial burst".to_owned());
    }

    // The seam starts at generation 1, so the first reset would exhaust it.
    let mut generation =
        Probe::with_policy(TerminalImageProcessPolicy::with_generation_ceiling_for_validation(1));
    generation.feed(&transmit_display(1))?;
    generation.converged()?;
    let definitions = generation.images.canonical_definitions();
    let placements = generation.images.canonical_placements();
    let scene = generation.committed();

    match generation.feed(&control("\x1bc")) {
        Err(error) if error == SessionTerminalError::GenerationExhausted.to_string() => {}
        other => return Err(format!("expected a typed generation rejection, got {other:?}")),
    }
    if generation.images.canonical_definitions() != definitions
        || generation.images.canonical_placements() != placements
    {
        return Err("generation exhaustion mutated canonical state".to_owned());
    }
    if generation.images.state().generation.0 != 1 {
        return Err("generation exhaustion advanced the generation".to_owned());
    }
    if !Arc::ptr_eq(&scene, &generation.committed()) {
        return Err("generation exhaustion published a burst".to_owned());
    }
    generation.converged()
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence<'_>) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode convergence evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
