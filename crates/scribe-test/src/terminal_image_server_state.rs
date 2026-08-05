//! Cross-invariant assembly evidence for the authoritative image state engine.
//!
//! One multi-session scenario drives framing, storage accounting, decode
//! scheduling, incomplete-transfer retirement, observer effects, transactional
//! mutations, client convergence, counter overflow, and session independence
//! through the same production seam, then publishes a versioned payload-free
//! manifest mapping every specification criterion to the case that proved it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use scribe_client::terminal_image_scene::{CommittedImageScene, LiveImageScene};
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{
    ImageLimits, TerminalImageDefinition, TerminalImageLiveMessage, TerminalImagePlacement,
    TerminalScreenKind,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_pty::graphics_framing::{GraphicsFailureCategory, GraphicsProtocol};
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_state::{
    ImageStorageOwnership, PtyTerminalImageState, SessionTerminalCommit, SessionTerminalError,
    SessionTerminalOutput, TerminalImageBoundary, TerminalImageProcessPolicy, TransferRetirement,
    feed_terminal_image_result_observed, observe_terminal_resize,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

/// Cell metrics used by every case so derived cell extents stay predictable.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

/// The manifest schema downstream `scribe-aq1.10` work reads.
const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// One 1x1 RGBA transfer split so neither read carries the whole image.
const SPLIT_FIRST_CHUNK: &str = "a=T,f=32,s=1,v=1,i=7,m=1";
const SPLIT_FIRST_PAYLOAD: &str = "/wAA";
const SPLIT_FINAL_PAYLOAD: &str = "gA==";

/// FNV-1a offset basis and prime for the stable convergence digest.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One ordered assembly stage and the closed check that proves it.
type NamedStage = (&'static str, fn(&mut Scenario) -> Result<(), String>);

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Versioned payload-free assembly evidence for the whole epic.
#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    status: &'static str,
    engine: &'static str,
    payload_free: bool,
    limits: ImageLimits,
    counters: Counters,
    typed_outcomes: BTreeMap<&'static str, String>,
    convergence: BTreeMap<&'static str, Convergence>,
    cases: BTreeMap<&'static str, &'static str>,
    child_gates: Vec<&'static str>,
    criteria: BTreeMap<&'static str, Criterion>,
}

/// One specification criterion, the assembly case that exercised it, and the
/// child functional gate that certifies it independently.
#[derive(Serialize)]
struct Criterion {
    case: &'static str,
    gate: &'static str,
    status: &'static str,
}

/// Canonical server and client digests for one session.
#[derive(Serialize)]
struct Convergence {
    server: String,
    client: String,
    converged: bool,
}

/// Exact counters the assembled engine ended the scenario with.
#[derive(Serialize)]
struct Counters {
    session_a: SessionCounters,
    session_b: SessionCounters,
    process_requested_current: u64,
    process_requested_peak: u64,
    scheduler: SchedulerCounters,
}

/// Payload-free per-session ownership and ordering counters.
#[derive(Serialize)]
struct SessionCounters {
    generation: u64,
    sequence: u64,
    definitions: usize,
    placements: usize,
    requested_current: u64,
    requested_peak: u64,
    observed_peak: u64,
    reserve_before_allocation_calls: u64,
    observed_reconciliations: u64,
    retained_bytes: usize,
    direct_reads: u64,
    speculative_clone_reads: u64,
}

/// Mandatory-scheduler admission counters after the whole scenario.
#[derive(Serialize)]
struct SchedulerCounters {
    issued: u64,
    admitted: u64,
    released: u64,
    rejected: u64,
    cancelled: u64,
    queued: u32,
    active: u32,
    peak_active: u32,
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

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

/// One production session: the server seam, the real Alacritty terminal that
/// observes it, and the production client scene it publishes to.
struct Probe {
    images: PtyTerminalImageState,
    term: Term<ScribeEventListener>,
    processor: Processor,
    scene: LiveImageScene,
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
            _event_rx: event_rx,
        }
    }

    /// Drive one PTY read through framing, the real terminal, the canonical
    /// mutation commit, and publication, then apply the burst to the client.
    fn feed(&mut self, bytes: &[u8]) -> Result<ReadOutcome, String> {
        let mut result = self.images.process_bytes(bytes);
        feed_terminal_image_result_observed(
            &mut self.images,
            &mut self.term,
            &mut self.processor,
            bytes,
            &mut result,
        );
        let commit = result.map_err(|error| error.to_string())?;
        let outcome = ReadOutcome::from_commit(&commit);
        let messages = self
            .images
            .commit_and_publish(&commit, &mut definition_payload)
            .map_err(|error| error.to_string())?;
        self.apply(messages)?;
        Ok(outcome)
    }

    /// Retire every incomplete transfer through the production close path.
    fn retire(&mut self, retirement: TransferRetirement) -> Result<ReadOutcome, String> {
        let commit = self.images.retire_transfers(retirement).map_err(|error| error.to_string())?;
        let outcome = ReadOutcome::from_commit(&commit);
        let messages = self
            .images
            .commit_and_publish(&commit, &mut definition_payload)
            .map_err(|error| error.to_string())?;
        self.apply(messages)?;
        Ok(outcome)
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
        for message in messages {
            self.scene.apply(message).map_err(|error| format!("client apply: {error}"))?;
        }
        Ok(())
    }

    fn committed(&self) -> Arc<CommittedImageScene> {
        self.scene.committed()
    }

    /// The active screen the real Alacritty terminal is on.
    fn terminal_screen(&self) -> TerminalScreenKind {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            TerminalScreenKind::Alternate
        } else {
            TerminalScreenKind::Primary
        }
    }

    fn placement_rows(&self) -> BTreeMap<u64, i32> {
        self.images
            .canonical_placements()
            .iter()
            .map(|(_, placement)| (placement.image_id.0, placement.anchor.row))
            .collect()
    }

    /// Stable payload-free digest of the server's canonical model.
    fn server_digest(&self) -> String {
        let state = self.images.state();
        digest(&format!(
            "{:?}|{:?}|{:?}|{:?}",
            state.generation,
            state.active_screen,
            sorted_definitions(self.images.canonical_definitions()),
            sorted_placements(self.images.canonical_placements()),
        ))
    }

    /// The same digest derived from the production client scene.
    fn client_digest(&self) -> String {
        let scene = self.committed();
        let state = self.images.state();
        // An empty scene never received a burst, so it carries no generation
        // of its own; the server's committed generation is then authoritative.
        let generation = scene.generation.unwrap_or(state.generation);
        let mut pairs: Vec<(TerminalScreenKind, TerminalImagePlacement)> = Vec::new();
        pairs.extend(
            scene
                .primary_placements
                .iter()
                .map(|placement| (TerminalScreenKind::Primary, placement.clone())),
        );
        pairs.extend(
            scene
                .alternate_placements
                .iter()
                .map(|placement| (TerminalScreenKind::Alternate, placement.clone())),
        );
        digest(&format!(
            "{:?}|{:?}|{:?}|{:?}",
            generation,
            scene.active_screen,
            sorted_definitions(
                scene.definitions.iter().map(|entry| entry.metadata.clone()).collect()
            ),
            sorted_placements(pairs),
        ))
    }

    /// Both canonical models agree, byte for byte, on every image fact.
    fn converged(&self) -> Result<(), String> {
        if self.server_digest() == self.client_digest() {
            return Ok(());
        }
        Err(format!(
            "canonical models diverged: server definitions {:?} placements {:?} screen {:?}; \
             client definitions {:?} placements {:?} screen {:?}",
            self.images.canonical_definitions(),
            self.images.canonical_placements(),
            self.images.state().active_screen,
            self.committed().definitions.len(),
            self.committed().placements().len(),
            self.committed().active_screen,
        ))
    }

    fn convergence(&self) -> Convergence {
        let server = self.server_digest();
        let client = self.client_digest();
        Convergence { converged: server == client, server, client }
    }

    fn counters(&self) -> Result<SessionCounters, String> {
        // `storage_counters` returns the session ledger first, then the
        // process ledger every session on this policy shares.
        let (session, _process) =
            self.images.storage_counters().map_err(|error| format!("counters: {error}"))?;
        let state = self.images.state();
        let work = self.images.framing_work();
        Ok(SessionCounters {
            generation: state.generation.0,
            sequence: state.sequence.0,
            definitions: state.definition_count,
            placements: state.placement_count,
            requested_current: session.requested_current,
            requested_peak: session.requested_peak,
            observed_peak: session.observed_peak,
            reserve_before_allocation_calls: session.reserve_before_allocation_calls,
            observed_reconciliations: session.observed_reconciliations,
            retained_bytes: retained_bytes(self.images.storage_ownership()),
            direct_reads: work.direct_reads,
            speculative_clone_reads: work.speculative_clone_reads,
        })
    }
}

/// What one production read published, payload-free.
struct ReadOutcome {
    /// Ordered outputs as `raw` or `image` in original PTY byte order.
    order: Vec<&'static str>,
    published_images: usize,
    failures: Vec<(&'static str, &'static str)>,
}

impl ReadOutcome {
    fn from_commit(commit: &SessionTerminalCommit) -> Self {
        let mut order = Vec::new();
        let mut published_images = 0;
        let mut failures = Vec::new();
        for output in commit.outputs.as_slice() {
            let SessionTerminalOutput::Image { boundary, .. } = output else {
                order.push("raw");
                continue;
            };
            order.push("image");
            match boundary {
                TerminalImageBoundary::Kitty { decoded: Some(_), .. }
                | TerminalImageBoundary::Sixel { .. } => published_images += 1,
                TerminalImageBoundary::Failure(failure) => failures
                    .push((category_name(failure.category), protocol_name(failure.protocol))),
                TerminalImageBoundary::Kitty { .. } | TerminalImageBoundary::SixelMode { .. } => {}
            }
        }
        Self { order, published_images, failures }
    }

    fn failure(&self) -> &'static str {
        self.failures.first().map_or("none", |(category, _)| *category)
    }

    fn failure_protocol(&self) -> &'static str {
        self.failures.first().map_or("none", |(_, protocol)| *protocol)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Canonical bytes for one published definition.
///
/// The server seam is payload-free by design, so the caller that owns decoded
/// pixels supplies them. Deterministic filler keeps this gate about ordering
/// and convergence rather than decoder output.
fn definition_payload(definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
    let length = usize::try_from(definition.rgba_bytes).ok()?;
    Some(vec![u8::try_from(definition.id.0 % 251).unwrap_or(0); length])
}

fn digest(text: &str) -> String {
    let value = text
        .as_bytes()
        .iter()
        .fold(FNV_OFFSET, |digest, byte| (digest ^ u64::from(*byte)).wrapping_mul(FNV_PRIME));
    format!("{value:016x}")
}

/// Framing-level partial sequences plus buffered Kitty chunk state.
fn pending_transfers(probe: &Probe) -> usize {
    usize::from(probe.images.state().pending_transfer.is_some())
        + usize::from(probe.images.validation_pending_kitty_decode_state().is_some())
}

fn retained_bytes(ownership: ImageStorageOwnership) -> usize {
    ownership.pending_kitty_requested
        + ownership.completed_kitty_requested
        + ownership.sixel_body_requested
        + ownership.kitty_decoded_requested
        + ownership.sixel_decoded_requested
}

const fn category_name(category: GraphicsFailureCategory) -> &'static str {
    match category {
        GraphicsFailureCategory::TruncatedSequence => "truncated_sequence",
        GraphicsFailureCategory::MalformedFraming => "malformed_framing",
        GraphicsFailureCategory::MalformedControl => "malformed_control",
        GraphicsFailureCategory::MalformedPayload => "malformed_payload",
        GraphicsFailureCategory::QuotaExceeded => "quota_exceeded",
        GraphicsFailureCategory::UnsupportedAction => "unsupported_action",
        GraphicsFailureCategory::UnsupportedProtocol => "unsupported_protocol",
        GraphicsFailureCategory::UnsupportedTransport => "unsupported_transport",
    }
}

const fn protocol_name(protocol: GraphicsProtocol) -> &'static str {
    match protocol {
        GraphicsProtocol::Kitty => "kitty",
        GraphicsProtocol::Sixel => "sixel",
    }
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

/// Transmit-and-display one 1x1 RGBA image under an explicit identifier.
fn transmit_display(image_id: u32) -> Vec<u8> {
    format!("\x1b_Ga=T,f=32,s=1,v=1,i={image_id};/wAAgA==\x1b\\").into_bytes()
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

/// Two independent production sessions on one shared process policy.
struct Scenario {
    a: Probe,
    b: Probe,
    typed: BTreeMap<&'static str, String>,
}

impl Scenario {
    fn new() -> Self {
        Self { a: Probe::new(), b: Probe::new(), typed: BTreeMap::new() }
    }

    fn record(&mut self, name: &'static str, outcome: impl Into<String>) {
        self.typed.insert(name, outcome.into());
    }
}

/// Framing keeps raw bytes ahead of the image boundaries that follow them, and
/// a transfer split across two reads anchors at the final chunk's cursor.
fn stage_framing(scenario: &mut Scenario) -> Result<(), String> {
    let first = scenario.a.feed(
        format!("\x1b[3;5Hhi\x1b_G{SPLIT_FIRST_CHUNK};{SPLIT_FIRST_PAYLOAD}\x1b\\").as_bytes(),
    )?;
    if first.order.last() != Some(&"image") || !first.order.contains(&"raw") {
        return Err(format!("raw bytes did not precede the image boundary: {:?}", first.order));
    }
    if first.published_images != 0 {
        return Err("a non-final chunk published an image".to_owned());
    }
    if pending_transfers(&scenario.a) == 0 {
        return Err("the split transfer left no pending state".to_owned());
    }

    let second =
        scenario.a.feed(format!("\x1b[6;2H\x1b_Gm=0;{SPLIT_FINAL_PAYLOAD}\x1b\\").as_bytes())?;
    if second.published_images != 1 {
        return Err("the final chunk did not publish exactly one image".to_owned());
    }
    let rows = scenario.a.placement_rows();
    if rows.get(&7) != Some(&5) {
        return Err(format!("the placement did not use the final-chunk cursor: {rows:?}"));
    }
    scenario.record("split_transfer_boundary", "kitty_final_chunk");
    scenario.a.converged()
}

/// Every retained byte was reserved before allocation and reconciled against
/// the capacity the allocator actually returned.
fn stage_accounting(scenario: &mut Scenario) -> Result<(), String> {
    let counters = scenario.a.counters()?;
    if counters.reserve_before_allocation_calls == 0 {
        return Err("no allocation reserved capacity before allocating".to_owned());
    }
    if counters.observed_reconciliations == 0 {
        return Err("no allocation reconciled its observed capacity".to_owned());
    }
    if counters.requested_peak == 0 {
        return Err("the session ledger recorded no retained peak".to_owned());
    }
    if counters.requested_current > counters.requested_peak {
        return Err("current session storage exceeded its own peak".to_owned());
    }

    // A quota too small for one canonical image rejects before any mutation.
    let mut pressured =
        Probe::with_policy(TerminalImageProcessPolicy::with_storage_limits_for_validation(8, 8));
    let Err(rejection) = pressured.feed(&transmit_display(9)) else {
        return Err("an exhausted quota still retained an image".to_owned());
    };
    if !pressured.images.canonical_definitions().is_empty()
        || !pressured.images.canonical_placements().is_empty()
    {
        return Err("a rejected reservation mutated canonical state".to_owned());
    }
    if retained_bytes(pressured.images.storage_ownership()) != 0 {
        return Err("a rejected reservation retained storage".to_owned());
    }
    scenario.record("storage_quota_rejection", rejection);
    Ok(())
}

/// Decode admission is mandatory, process-owned, and fully released.
fn stage_scheduling(scenario: &mut Scenario) -> Result<(), String> {
    let metrics = TerminalImageProcessPolicy::v1()
        .decode_scheduler()
        .metrics()
        .map_err(|error| format!("scheduler metrics: {error:?}"))?;
    if metrics.issued == 0 || metrics.admitted == 0 {
        return Err("no decode reached the mandatory scheduler".to_owned());
    }
    if metrics.released != metrics.admitted {
        return Err(format!(
            "admissions outlived their decodes: admitted {} released {}",
            metrics.admitted, metrics.released
        ));
    }
    if metrics.active != 0 || metrics.queued != 0 {
        return Err("a decode admission survived the scenario".to_owned());
    }
    let limits = TerminalImageProcessPolicy::v1().limits();
    if metrics.peak_active > limits.max_concurrent_decodes {
        return Err("concurrent decodes exceeded the process ceiling".to_owned());
    }
    scenario.record("decode_admission", "scheduler_bound");
    Ok(())
}

/// Terminal chronology follows the real Alacritty grid across scroll regions,
/// screen swaps, and a resize that clips both grids.
fn stage_observer(scenario: &mut Scenario) -> Result<(), String> {
    // A margin scroll moves the anchor of the placement it contains.
    scenario.a.feed(b"\x1b[3;8r\x1b[1S")?;
    let rows = scenario.a.placement_rows();
    if rows.get(&7) != Some(&4) {
        return Err(format!("the margin scroll did not move the placement: {rows:?}"));
    }
    scenario.a.converged()?;

    // The seam's active screen is the terminal's active screen.
    scenario.a.feed(b"\x1b[?1049h")?;
    if scenario.a.images.state().active_screen != scenario.a.terminal_screen() {
        return Err("the seam and the terminal disagree on the active screen".to_owned());
    }
    scenario.a.feed(&[b"\x1b[8;1H".to_vec(), transmit_display(11)].concat())?;
    scenario.a.feed(b"\x1b[?1049l")?;
    if scenario.a.images.state().active_screen != scenario.a.terminal_screen() {
        return Err("leaving the alternate screen desynchronized the seam".to_owned());
    }
    scenario.a.converged()?;

    // One resize clips the active and inactive grids alike.
    let before = scenario.a.images.canonical_placements().len();
    scenario.a.resize(40, 4)?;
    let after = scenario.a.images.canonical_placements();
    if after.len() >= before {
        return Err("the resize clipped no out-of-bounds placement".to_owned());
    }
    if after.iter().any(|(_, placement)| placement.anchor.row >= 4) {
        return Err(format!("the resize left an out-of-bounds placement: {after:?}"));
    }
    scenario.record("resize_clipping", "both_grids");
    scenario.a.converged()
}

/// Definitions, placements, deletes, and eviction commit atomically and stay
/// scoped to the screen and target the command named.
fn stage_mutations(scenario: &mut Scenario) -> Result<(), String> {
    scenario.a.feed(b"\x1bc")?;
    scenario.a.feed(&[b"\x1b[2;2H".to_vec(), transmit_display(21)].concat())?;
    scenario.a.feed(&[b"\x1b[3;2H".to_vec(), transmit_display(22)].concat())?;
    if scenario.a.images.canonical_definitions().len() != 2 {
        return Err("the compound transmits did not define two images".to_owned());
    }
    if scenario.a.images.canonical_placements().len() != 2 {
        return Err("the compound transmits did not place two images".to_owned());
    }

    // A soft delete removes placements only; a hard delete frees the image.
    scenario.a.feed(b"\x1b_Ga=d,d=i,i=21\x1b\\")?;
    if scenario.a.images.canonical_definitions().len() != 2 {
        return Err("a soft delete freed an image definition".to_owned());
    }
    scenario.a.converged()?;
    scenario.a.feed(b"\x1b_Ga=d,d=I,i=21\x1b\\")?;
    if scenario.a.images.canonical_definitions().len() != 1 {
        return Err("a hard delete did not free its image definition".to_owned());
    }
    scenario.a.converged()?;

    // A malformed operand fails as typed protocol input and mutates nothing.
    let before_definitions = scenario.a.images.canonical_definitions();
    let before_placements = scenario.a.images.canonical_placements();
    let malformed = scenario.a.feed(b"\x1b_Ga=T,f=32,s=0,v=0,i=23;/wAAgA==\x1b\\")?;
    if malformed.failure() == "none" {
        return Err("a malformed transmit produced no typed failure".to_owned());
    }
    if scenario.a.images.canonical_definitions() != before_definitions
        || scenario.a.images.canonical_placements() != before_placements
    {
        return Err("a failed command mutated canonical state".to_owned());
    }
    scenario.record("malformed_operand", malformed.failure());
    scenario.record("malformed_operand_protocol", malformed.failure_protocol());

    // Filling the session image ceiling evicts oldest-first, and the client
    // hears the eviction before the definition that displaced it.
    let ceiling = TerminalImageProcessPolicy::v1().limits().max_images_per_session;
    for image_id in 100..=(100 + ceiling) {
        scenario.a.feed(&transmit_display(image_id))?;
    }
    let definitions = scenario.a.images.canonical_definitions();
    if definitions.len() != ceiling as usize {
        return Err(format!(
            "eviction did not hold the session ceiling: {} of {ceiling}",
            definitions.len()
        ));
    }
    scenario.record("image_eviction", "oldest_first");
    scenario.a.converged()
}

/// An incomplete transfer on a second session retires without publishing.
fn stage_retirement(scenario: &mut Scenario) -> Result<(), String> {
    scenario.b.feed(b"\x1b[2;2Hbefore")?;
    let before = scenario.b.images.state().generation;
    scenario.b.feed(format!("\x1b_G{SPLIT_FIRST_CHUNK};{SPLIT_FIRST_PAYLOAD}\x1b\\").as_bytes())?;
    if pending_transfers(&scenario.b) == 0 {
        return Err("the abandoned transfer left no pending state".to_owned());
    }

    let retired = scenario.b.retire(TransferRetirement::Close)?;
    if retired.published_images != 0 {
        return Err("a retired transfer published an image".to_owned());
    }
    if retired.failure() != "truncated_sequence" {
        return Err(format!("the retirement was not typed truncated: {}", retired.failure()));
    }
    let state = scenario.b.images.state();
    if pending_transfers(&scenario.b) != 0 {
        return Err("close left pending transfer state".to_owned());
    }
    if state.definition_count != 0 || state.placement_count != 0 {
        return Err("an incomplete transfer became canonical state".to_owned());
    }
    if state.generation != before {
        return Err("an incomplete transfer consumed a generation".to_owned());
    }
    if retained_bytes(scenario.b.images.storage_ownership()) != 0 {
        return Err("a retired transfer retained storage".to_owned());
    }

    // Repeating the retirement is idempotent and cannot underflow accounting.
    let repeated = scenario.b.retire(TransferRetirement::Close)?;
    if !repeated.order.is_empty() {
        return Err("a repeated close produced output".to_owned());
    }
    let counters = scenario.b.counters()?;
    if counters.requested_current != 0 {
        return Err("session storage survived retirement".to_owned());
    }
    scenario.record("incomplete_transfer_retirement", retired.failure());
    scenario.record("incomplete_transfer_protocol", retired.failure_protocol());
    scenario.b.converged()
}

/// Exhausting the sequence or generation ceiling rejects before any mutation
/// and leaves the last committed state and published scene untouched.
fn stage_overflow(scenario: &mut Scenario) -> Result<(), String> {
    let mut sequence =
        Probe::with_policy(TerminalImageProcessPolicy::with_sequence_ceiling_for_validation(1));
    let Err(sequence_rejection) = sequence.feed(&transmit_display(31)) else {
        return Err("an exhausted sequence still published".to_owned());
    };
    if sequence_rejection != SessionTerminalError::SequenceExhausted.to_string() {
        return Err(format!("sequence exhaustion was not typed: {sequence_rejection}"));
    }
    if !sequence.images.canonical_definitions().is_empty()
        || !sequence.images.canonical_placements().is_empty()
        || !sequence.committed().definitions.is_empty()
    {
        return Err("sequence exhaustion mutated or published state".to_owned());
    }
    scenario.record("sequence_overflow", sequence_rejection);

    let mut generation =
        Probe::with_policy(TerminalImageProcessPolicy::with_generation_ceiling_for_validation(1));
    generation.feed(&transmit_display(32))?;
    let definitions = generation.images.canonical_definitions();
    let scene = generation.committed();
    let Err(generation_rejection) = generation.feed(b"\x1bc") else {
        return Err("an exhausted generation still reset".to_owned());
    };
    if generation_rejection != SessionTerminalError::GenerationExhausted.to_string() {
        return Err(format!("generation exhaustion was not typed: {generation_rejection}"));
    }
    if generation.images.canonical_definitions() != definitions
        || generation.images.state().generation.0 != 1
        || !Arc::ptr_eq(&scene, &generation.committed())
    {
        return Err("generation exhaustion changed committed state".to_owned());
    }
    scenario.record("generation_overflow", generation_rejection);
    generation.converged()
}

/// Two sessions on one process policy keep disjoint canonical state while
/// sharing the process ceilings that bound them both.
fn stage_independent_sessions(scenario: &mut Scenario) -> Result<(), String> {
    if scenario.a.images.decode_session() == scenario.b.images.decode_session() {
        return Err("both sessions issued decodes under one identity".to_owned());
    }
    scenario.b.feed(&[b"\x1b[4;4H".to_vec(), transmit_display(41)].concat())?;
    let a_ids: Vec<u64> =
        scenario.a.images.canonical_definitions().iter().map(|entry| entry.id.0).collect();
    if a_ids.contains(&41) {
        return Err("one session observed another session's image".to_owned());
    }
    if scenario.b.images.canonical_definitions().len() != 1 {
        return Err("the second session did not retain its own image".to_owned());
    }

    let (_, a_process) =
        scenario.a.images.storage_counters().map_err(|error| format!("counters: {error}"))?;
    let (b_session, b_process) =
        scenario.b.images.storage_counters().map_err(|error| format!("counters: {error}"))?;
    if a_process != b_process {
        return Err("the sessions did not share one process ledger".to_owned());
    }
    if b_process.requested_current < b_session.requested_current {
        return Err("session storage escaped the process ledger".to_owned());
    }
    scenario.record("session_isolation", "disjoint_state_shared_process");
    scenario.a.converged()?;
    scenario.b.converged()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

// @lat: [[test#Test Harness#Authoritative Image State Assembly#Cross-Invariant Assembly Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let stages: [NamedStage; 7] = [
        ("framing_ordering", stage_framing),
        ("storage_accounting", stage_accounting),
        ("decode_scheduling", stage_scheduling),
        ("observer_effects", stage_observer),
        ("transactional_mutations", stage_mutations),
        ("incomplete_retirement", stage_retirement),
        ("counter_overflow", stage_overflow),
    ];
    let mut scenario = Scenario::new();
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, stage) in stages {
        stage(&mut scenario).map_err(|error| format!("{name}: {error}"))?;
        cases.insert(name, "pass");
    }
    // Retirement runs before independence so the second session's isolation is
    // measured after it has already been reset and closed once.
    stage_independent_sessions(&mut scenario)
        .map_err(|error| format!("independent_sessions: {error}"))?;
    cases.insert("independent_sessions", "pass");

    let convergence = BTreeMap::from([
        ("session_a", scenario.a.convergence()),
        ("session_b", scenario.b.convergence()),
    ]);
    if convergence.values().any(|entry| !entry.converged) {
        return Err("a session ended the scenario divergent".to_owned());
    }
    cases.insert("client_convergence", "pass");

    let (_, process) =
        scenario.a.images.storage_counters().map_err(|error| format!("counters: {error}"))?;
    let metrics = TerminalImageProcessPolicy::v1()
        .decode_scheduler()
        .metrics()
        .map_err(|error| format!("scheduler metrics: {error:?}"))?;
    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        status: "pass",
        engine: "scribe-server authoritative session terminal seam",
        payload_free: true,
        limits: TerminalImageProcessPolicy::v1().limits(),
        counters: Counters {
            session_a: scenario.a.counters()?,
            session_b: scenario.b.counters()?,
            process_requested_current: process.requested_current,
            process_requested_peak: process.requested_peak,
            scheduler: SchedulerCounters {
                issued: metrics.issued,
                admitted: metrics.admitted,
                released: metrics.released,
                rejected: metrics.rejected,
                cancelled: metrics.cancelled,
                queued: metrics.queued,
                active: metrics.active,
                peak_active: metrics.peak_active,
            },
        },
        typed_outcomes: scenario.typed.iter().map(|(name, value)| (*name, value.clone())).collect(),
        convergence,
        cases,
        child_gates: CHILD_GATES.to_vec(),
        criteria: criteria(),
    };
    write_manifest(evidence_path, &manifest)
}

/// The child functional gates whose independent evidence this assembly rests
/// on; each runs as its own `just e2e-func` case.
const CHILD_GATES: [&str; 7] = [
    "terminal-image-state-seam.sh",
    "terminal-image-accounting.sh",
    "terminal-image-scheduler.sh",
    "terminal-image-transfer-lifecycle.sh",
    "terminal-image-observer-parity.sh",
    "terminal-image-mutations.sh",
    "terminal-image-convergence.sh",
];

/// Map every specification acceptance criterion to the assembly case that
/// exercised it and the child gate that certifies it independently.
fn criteria() -> BTreeMap<&'static str, Criterion> {
    const ROWS: [(&str, &str, &str); 40] = [
        ("US1.1", "storage_accounting", "terminal-image-accounting.sh"),
        ("US1.2", "storage_accounting", "terminal-image-accounting.sh"),
        ("US1.3", "storage_accounting", "terminal-image-accounting.sh"),
        ("US1.4", "storage_accounting", "terminal-image-accounting.sh"),
        ("US1.5", "storage_accounting", "terminal-image-accounting.sh"),
        ("US2.1", "incomplete_retirement", "terminal-image-transfer-lifecycle.sh"),
        ("US2.2", "incomplete_retirement", "terminal-image-transfer-lifecycle.sh"),
        ("US2.3", "incomplete_retirement", "terminal-image-transfer-lifecycle.sh"),
        ("US2.4", "incomplete_retirement", "terminal-image-transfer-lifecycle.sh"),
        ("US2.5", "incomplete_retirement", "terminal-image-transfer-lifecycle.sh"),
        ("US3.1", "framing_ordering", "terminal-image-state-seam.sh"),
        ("US3.2", "framing_ordering", "terminal-image-state-seam.sh"),
        ("US3.3", "observer_effects", "terminal-image-observer-parity.sh"),
        ("US3.4", "observer_effects", "terminal-image-observer-parity.sh"),
        ("US3.5", "observer_effects", "terminal-image-observer-parity.sh"),
        ("US4.1", "client_convergence", "terminal-image-convergence.sh"),
        ("US4.2", "counter_overflow", "terminal-image-convergence.sh"),
        ("US4.3", "counter_overflow", "terminal-image-convergence.sh"),
        ("US4.4", "counter_overflow", "terminal-image-convergence.sh"),
        ("US4.5", "client_convergence", "terminal-image-convergence.sh"),
        ("US5.1", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.2", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.3", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.4", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.5", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.6", "decode_scheduling", "terminal-image-scheduler.sh"),
        ("US5.7", "independent_sessions", "terminal-image-scheduler.sh"),
        ("US6.1", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.2", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.3", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.4", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.5", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.6", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US6.7", "transactional_mutations", "terminal-image-mutations.sh"),
        ("US7.1", "independent_sessions", "terminal-image-server-state.sh"),
        ("US7.2", "framing_ordering", "terminal-image-server-state.sh"),
        ("US7.3", "client_convergence", "terminal-image-server-state.sh"),
        ("US7.4", "transactional_mutations", "terminal-image-server-state.sh"),
        ("US7.5", "client_convergence", "terminal-image-server-state.sh"),
        ("US7.6", "framing_ordering", "terminal-image-server-state.sh"),
    ];
    ROWS.into_iter()
        .map(|(id, case, gate)| (id, Criterion { case, gate, status: "pass" }))
        .collect()
}

fn write_manifest(evidence_path: &Path, manifest: &Manifest) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("encode server state manifest: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
