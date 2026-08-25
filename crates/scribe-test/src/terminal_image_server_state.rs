//! Cross-invariant evidence for the authoritative terminal-image state engine.
//!
//! Component probes own framing, accounting, scheduling, retirement, observer,
//! mutation, and counter assertions. This probe keeps only behavior visible
//! when those components compose: split-transfer publication at the final
//! observed cursor, cross-session eviction isolation on one process ledger,
//! and server/client convergence throughout.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use scribe_client::terminal_image_scene::{CommittedImageScene, LiveImageScene};
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{
    TerminalImageDefinition, TerminalImagePlacement, TerminalScreenKind,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, SessionTerminalCommit, SessionTerminalOutput, TerminalImageBoundary,
    TerminalImageProcessPolicy, feed_terminal_image_result_observed,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const SPLIT_FIRST_CHUNK: &str = "a=T,f=32,s=1,v=1,i=7,m=1";
const SPLIT_FIRST_PAYLOAD: &str = "/wAA";
const SPLIT_FINAL_PAYLOAD: &str = "gA==";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    status: &'static str,
    engine: &'static str,
    payload_free: bool,
    cases: BTreeMap<&'static str, &'static str>,
    cross_invariants: CrossInvariantEvidence,
    convergence: BTreeMap<&'static str, Convergence>,
}

#[derive(Serialize)]
struct CrossInvariantEvidence {
    split_transfer: SplitTransferEvidence,
    cross_session_eviction: CrossSessionEvictionEvidence,
}

#[derive(Serialize)]
struct SplitTransferEvidence {
    ordering: &'static str,
    pending_state: &'static str,
    published_on_final_chunk: usize,
    final_cursor_anchor_row: i32,
    convergence: &'static str,
}

#[derive(Serialize)]
struct CrossSessionEvictionEvidence {
    decode_identity: &'static str,
    evicted_image_id: u64,
    evicting_session_definitions: usize,
    isolated_session_definitions: usize,
    isolated_session_state: &'static str,
    process_ledger: &'static str,
    process_requested_current: u64,
    session_requested_total: u64,
    convergence: &'static str,
}

#[derive(Serialize)]
struct Convergence {
    server: String,
    client: String,
    converged: bool,
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

struct Probe {
    images: PtyTerminalImageState,
    term: Term<ScribeEventListener>,
    processor: Processor,
    scene: LiveImageScene,
    _event_rx: mpsc::UnboundedReceiver<SessionEvent>,
}

impl Probe {
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
        for message in messages {
            self.scene.apply(message).map_err(|error| format!("client apply: {error}"))?;
        }
        Ok(outcome)
    }

    fn committed(&self) -> Arc<CommittedImageScene> {
        self.scene.committed()
    }

    fn placement_rows(&self) -> BTreeMap<u64, i32> {
        self.images
            .canonical_placements()
            .iter()
            .map(|(_, placement)| (placement.image_id.0, placement.anchor.row))
            .collect()
    }

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

    fn client_digest(&self) -> String {
        let scene = self.committed();
        let state = self.images.state();
        let generation = scene.generation.unwrap_or(state.generation);
        let mut placements: Vec<(TerminalScreenKind, TerminalImagePlacement)> = Vec::new();
        placements.extend(
            scene
                .primary_placements
                .iter()
                .map(|placement| (TerminalScreenKind::Primary, placement.clone())),
        );
        placements.extend(
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
            sorted_placements(placements),
        ))
    }

    fn converged(&self) -> bool {
        self.server_digest() == self.client_digest()
    }

    fn convergence(&self) -> Convergence {
        let server = self.server_digest();
        let client = self.client_digest();
        Convergence { converged: server == client, server, client }
    }
}

struct ReadOutcome {
    order: Vec<&'static str>,
    published_images: usize,
}

impl ReadOutcome {
    fn from_commit(commit: &SessionTerminalCommit) -> Self {
        let mut order = Vec::new();
        let mut published_images = 0;
        for output in commit.outputs.as_slice() {
            let SessionTerminalOutput::Image { boundary, .. } = output else {
                order.push("raw");
                continue;
            };
            order.push("image");
            if matches!(
                boundary,
                TerminalImageBoundary::Kitty { decoded: Some(_), .. }
                    | TerminalImageBoundary::Sixel { .. }
            ) {
                published_images += 1;
            }
        }
        Self { order, published_images }
    }
}

fn definition_payload(definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
    let length = usize::try_from(definition.rgba_bytes).ok()?;
    Some(vec![u8::try_from(definition.id.0 % 251).unwrap_or(0); length])
}

fn pending_transfers(probe: &Probe) -> usize {
    usize::from(probe.images.state().pending_transfer.is_some())
        + usize::from(probe.images.validation_pending_kitty_decode_state().is_some())
}

fn transmit_display(image_id: u32) -> Vec<u8> {
    format!("\x1b_Ga=T,f=32,s=1,v=1,i={image_id};/wAAgA==\x1b\\").into_bytes()
}

fn digest(text: &str) -> String {
    let value = text
        .as_bytes()
        .iter()
        .fold(FNV_OFFSET, |digest, byte| (digest ^ u64::from(*byte)).wrapping_mul(FNV_PRIME));
    format!("{value:016x}")
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

fn verify_split_transfer(probe: &mut Probe) -> Result<SplitTransferEvidence, String> {
    let first = probe.feed(
        format!("\x1b[3;5Hhi\x1b_G{SPLIT_FIRST_CHUNK};{SPLIT_FIRST_PAYLOAD}\x1b\\").as_bytes(),
    )?;
    let raw_preceded_image_boundary =
        first.order.last() == Some(&"image") && first.order.contains(&"raw");
    if !raw_preceded_image_boundary || first.published_images != 0 {
        return Err(format!("split transfer first read drifted: {:?}", first.order));
    }
    let pending_after_first_chunk = pending_transfers(probe) > 0;
    if !pending_after_first_chunk {
        return Err("split transfer left no pending state".to_owned());
    }

    let second =
        probe.feed(format!("\x1b[6;2H\x1b_Gm=0;{SPLIT_FINAL_PAYLOAD}\x1b\\").as_bytes())?;
    let final_cursor_anchor_row = probe
        .placement_rows()
        .get(&7)
        .copied()
        .ok_or_else(|| "split transfer published no placement".to_owned())?;
    let converged_after_publication = probe.converged();
    if second.published_images != 1 || final_cursor_anchor_row != 5 || !converged_after_publication
    {
        return Err(format!(
            "split transfer final publication drifted: images={} row={} converged={}",
            second.published_images, final_cursor_anchor_row, converged_after_publication
        ));
    }

    Ok(SplitTransferEvidence {
        ordering: "raw_before_image_boundary",
        pending_state: "first_chunk_only",
        published_on_final_chunk: second.published_images,
        final_cursor_anchor_row,
        convergence: "server_client",
    })
}

fn verify_cross_session_eviction(
    session_a: &mut Probe,
    session_b: &mut Probe,
) -> Result<CrossSessionEvictionEvidence, String> {
    session_b.feed(&[b"\x1b[4;4H".to_vec(), transmit_display(41)].concat())?;
    let session_b_server_before = session_b.server_digest();
    let session_b_client_before = session_b.client_digest();

    let ceiling = TerminalImageProcessPolicy::v1().limits().max_images_per_session;
    for image_id in 100..100 + ceiling {
        session_a.feed(&transmit_display(image_id))?;
    }

    let evicting_definitions = session_a.images.canonical_definitions();
    let isolated_definitions = session_b.images.canonical_definitions();
    let session_b_unchanged = session_b.server_digest() == session_b_server_before
        && session_b.client_digest() == session_b_client_before;
    let distinct_decode_sessions =
        session_a.images.decode_session() != session_b.images.decode_session();
    if evicting_definitions.len() != ceiling as usize
        || evicting_definitions.iter().any(|definition| definition.id.0 == 7)
        || isolated_definitions.len() != 1
        || isolated_definitions.first().map(|definition| definition.id.0) != Some(41)
        || !session_b_unchanged
        || !distinct_decode_sessions
    {
        return Err("cross-session eviction changed the wrong canonical state".to_owned());
    }

    let (evicting_storage, process_counters) = session_a
        .images
        .storage_counters()
        .map_err(|error| format!("session A counters: {error}"))?;
    let (isolated_storage, isolated_process) = session_b
        .images
        .storage_counters()
        .map_err(|error| format!("session B counters: {error}"))?;
    let session_requested_total = evicting_storage
        .requested_current
        .checked_add(isolated_storage.requested_current)
        .ok_or_else(|| "session storage total overflowed".to_owned())?;
    let shared_process_ledger = process_counters == isolated_process
        && process_counters.requested_current == session_requested_total;
    let both_clients_converged = session_a.converged() && session_b.converged();
    if !shared_process_ledger || !both_clients_converged {
        return Err(format!(
            "composed state drifted: shared_ledger={shared_process_ledger} converged={both_clients_converged}"
        ));
    }

    Ok(CrossSessionEvictionEvidence {
        decode_identity: "distinct",
        evicted_image_id: 7,
        evicting_session_definitions: evicting_definitions.len(),
        isolated_session_definitions: isolated_definitions.len(),
        isolated_session_state: "unchanged",
        process_ledger: "shared_exact_sum",
        process_requested_current: process_counters.requested_current,
        session_requested_total,
        convergence: "both_clients",
    })
}

// @lat: [[test#Test Harness#Authoritative Image State Assembly#Cross-Invariant Assembly Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let policy = TerminalImageProcessPolicy::v1();
    let mut session_a = Probe::with_policy(Arc::clone(&policy));
    let mut session_b = Probe::with_policy(policy);

    let split_transfer = verify_split_transfer(&mut session_a)?;
    let cross_session_eviction = verify_cross_session_eviction(&mut session_a, &mut session_b)?;
    let convergence = BTreeMap::from([
        ("session_a", session_a.convergence()),
        ("session_b", session_b.convergence()),
    ]);
    if convergence.values().any(|entry| !entry.converged) {
        return Err("a session ended the cross-invariant scenario divergent".to_owned());
    }

    let manifest = Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        status: "pass",
        engine: "scribe-server authoritative session terminal seam",
        payload_free: true,
        cases: BTreeMap::from([
            ("split_transfer_publication", "pass"),
            ("cross_session_eviction_isolation", "pass"),
            ("client_convergence", "pass"),
        ]),
        cross_invariants: CrossInvariantEvidence { split_transfer, cross_session_eviction },
        convergence,
    };
    write_manifest(evidence_path, &manifest)
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
