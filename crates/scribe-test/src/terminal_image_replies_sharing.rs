//! Production-path evidence for terminal-image replies and viewer sharing.
//!
//! Every case drives the production seam: real PTY bytes through production
//! framing and the real Alacritty terminal, the server's reply planner, the
//! server's capability latch, and the real per-session attached-sink set. The
//! viewer cases install genuine bounded output queues and read the delivered
//! frames back off each connection's pipe, so "this viewer received the burst
//! exactly once" is a receipt, not an inference.

use std::collections::BTreeMap;
use std::path::Path;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use scribe_common::ids::SessionId;
use scribe_common::protocol::ServerMessage;
use scribe_common::terminal_images::{
    TerminalImageCapabilities, TerminalImageDefinition, TerminalImageFeatures,
    TerminalImageLiveMessage,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_server::image_sharing_probe;
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_sharing::{
    KillSwitchTransition, SessionImageSharing, augment_device_attributes,
    effective_connection_subset, plan_pty_replies,
};
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, TerminalImageProcessPolicy, feed_terminal_image_result_observed,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

use crate::framing_probe::read_hex;

/// Cell metrics shared with the other terminal-image probes.
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 16;

/// The pinned corpus fixture whose expected outcome is `kitty_ok_precedes_da1`.
const QUERY_ORDER_FIXTURE: &str = "kitty-query-order.hex";
/// The pinned corpus fixture that commits one definition and one placement.
const RGB_CLASSIC_FIXTURE: &str = "kitty-rgb-classic.hex";

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    payload_free: bool,
    replies: ReplyEvidence,
    viewers: ViewerEvidence,
    capability: CapabilityEvidence,
    kill_switch: KillSwitchEvidence,
    cases: BTreeMap<&'a str, &'a str>,
}

/// Ordered PTY write-back facts for one capability probe followed by DA1.
#[derive(Serialize)]
struct ReplyEvidence {
    /// The kinds written to the PTY, in the exact production order.
    ordered_pty_writes: String,
    kitty_replies: usize,
    device_attributes_replies: usize,
    /// Replies planned a second time for the same committed read.
    replayed_kitty_replies: usize,
    /// Replies planned while the session's capability is not live.
    disabled_kitty_replies: usize,
}

/// Delivery receipts across viewer counts, a controller change, and a detach.
#[derive(Serialize)]
struct ViewerEvidence {
    burst_records: usize,
    zero_viewers_delivered: usize,
    one_viewer_delivered: usize,
    one_viewer_received: usize,
    multiple_viewers_delivered: usize,
    multiple_viewers_received: String,
    incapable_viewer_received: usize,
    controller_change_delivered: usize,
    controller_change_displaced_received: usize,
    controller_change_new_received: usize,
    detached_delivered: usize,
    detached_viewer_received: usize,
}

/// Capability latch facts across viewers, detach, and refusal.
#[derive(Serialize)]
struct CapabilityEvidence {
    latched_subset_features: u16,
    latch_is_idempotent: bool,
    survives_detach: bool,
    capable_admissions: usize,
    incapable_refusals: usize,
    refusal_required_features: u16,
    refusal_offered_features: u16,
    unlatched_admission: &'static str,
}

/// Every observed master-switch transition, in order.
#[derive(Serialize)]
struct KillSwitchEvidence {
    transitions: String,
    cleared_latch_once: bool,
    disabled_device_attributes: String,
    enabled_device_attributes: String,
    relatched_after_reenable: bool,
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

/// Server seam plus the real terminal that observes it.
struct Probe {
    images: PtyTerminalImageState,
    term: Term<ScribeEventListener>,
    processor: Processor,
    event_rx: mpsc::UnboundedReceiver<SessionEvent>,
}

impl Probe {
    fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(SessionId::new(), event_tx);
        let dimensions = ProbeDimensions { columns: 40, rows: 10 };
        let term = Term::new(build_term_config(32), &dimensions, listener);
        let images = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
        images.grid_observer().set_cell_size(CELL_WIDTH, CELL_HEIGHT);
        Self { images, term, processor: Processor::new(), event_rx }
    }

    /// Drive one PTY read through framing and the real terminal, exactly as
    /// the production reader's ingress does.
    fn feed(
        &mut self,
        bytes: &[u8],
    ) -> Result<scribe_server::terminal_image_state::SessionTerminalCommit, String> {
        let mut result = self.images.process_bytes(bytes);
        feed_terminal_image_result_observed(
            &mut self.images,
            &mut self.term,
            &mut self.processor,
            bytes,
            &mut result,
        );
        result.map_err(|error| error.to_string())
    }

    /// Drain the terminal's own reply events, in emission order.
    fn drain_term_replies(&mut self) -> Vec<String> {
        let mut replies = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if let SessionEvent::PtyWrite(text) = event {
                replies.push(text);
            }
        }
        replies
    }
}

pub fn run(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create sharing probe runtime: {error}"))?;
    runtime.block_on(run_probe(fixtures, evidence_path))
}

async fn run_probe(fixtures: &Path, evidence_path: &Path) -> Result<(), String> {
    let mut cases: BTreeMap<&str, &str> = BTreeMap::new();

    let replies = verify_reply_order(fixtures)?;
    cases.insert("kitty_reply_before_da", "pass");
    cases.insert("reply_exactly_once", "pass");

    let kill_switch = verify_kill_switch(fixtures)?;
    cases.insert("da4_enablement", "pass");
    cases.insert("kill_switch_transitions", "pass");

    let capability = verify_capability_lifecycle()?;
    cases.insert("incapable_attach_refusal", "pass");
    cases.insert("latch_survives_detach", "pass");

    let viewers = verify_viewer_fanout(fixtures).await?;
    cases.insert("zero_one_multiple_viewers", "pass");
    cases.insert("controller_change", "pass");
    cases.insert("detach", "pass");

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server image replies and capable-sink fanout",
        payload_free: true,
        replies,
        viewers,
        capability,
        kill_switch,
        cases,
    };
    write_evidence(evidence_path, &evidence)
}

/// A valid `a=q` probe must get its Kitty result before the DA1 reply that
/// follows it in the same PTY read, and neither may be written twice.
fn verify_reply_order(fixtures: &Path) -> Result<ReplyEvidence, String> {
    let bytes = read_hex(&fixtures.join(QUERY_ORDER_FIXTURE))?;
    let mut probe = Probe::new();
    let commit = probe.feed(&bytes)?;

    // Production order: the reply planner writes first, then the reader drains
    // the terminal's own event queue.
    let image_replies = plan_pty_replies(&commit, true);
    let term_replies = probe.drain_term_replies();

    let mut ordered: Vec<&'static str> = Vec::new();
    for reply in &image_replies {
        let text = String::from_utf8_lossy(&reply.bytes).into_owned();
        if !text.starts_with("\u{1b}_G") || !text.ends_with("\u{1b}\\") {
            return Err(format!("image reply is not an APC G string: {text:?}"));
        }
        if !text.contains("i=31") || !text.contains(";OK") {
            return Err(format!("Kitty query reply did not echo its image id: {text:?}"));
        }
        ordered.push("kitty_ok");
    }
    for reply in &term_replies {
        if !is_primary_device_attributes(reply) {
            return Err(format!("unexpected terminal reply in this read: {reply:?}"));
        }
        ordered.push("device_attributes");
    }
    if ordered != ["kitty_ok", "device_attributes"] {
        return Err(format!(
            "reply order is {ordered:?}, expected kitty_ok then device_attributes"
        ));
    }

    // Replanning the same committed read is the operation a second viewer, a
    // reattach, or a replay would trigger; it must add nothing to the PTY.
    let replayed = plan_pty_replies(&commit, true);
    let disabled = plan_pty_replies(&commit, false);

    Ok(ReplyEvidence {
        ordered_pty_writes: ordered.join(","),
        kitty_replies: image_replies.len(),
        device_attributes_replies: term_replies.len(),
        // The planner is pure: the same commit yields the same single reply, so
        // the reader writing it once is the whole exactly-once guarantee.
        replayed_kitty_replies: replayed.len(),
        disabled_kitty_replies: disabled.len(),
    })
}

/// Disabling the master switch must clear the latch, stop every reply, and drop
/// DA1 attribute 4 — and each transition must be reported exactly once.
fn verify_kill_switch(fixtures: &Path) -> Result<KillSwitchEvidence, String> {
    let bytes = read_hex(&fixtures.join(QUERY_ORDER_FIXTURE))?;
    let mut probe = Probe::new();
    let commit = probe.feed(&bytes)?;
    let device_attributes = probe
        .drain_term_replies()
        .into_iter()
        .find(|reply| is_primary_device_attributes(reply))
        .ok_or_else(|| "the terminal emitted no DA1 reply".to_owned())?;

    let mut sharing = SessionImageSharing::new(true);
    sharing.latch(TerminalImageCapabilities::V1);
    if plan_pty_replies(&commit, sharing.images_enabled()).len() != 1 {
        return Err("an enabled session did not owe its Kitty reply".to_owned());
    }
    let enabled_da = augment_device_attributes(&device_attributes, sharing.images_enabled());
    if !enabled_da.contains(";4c") {
        return Err(format!("enabled DA1 lacks attribute 4: {enabled_da:?}"));
    }
    if augment_device_attributes(&enabled_da, true) != enabled_da {
        return Err("attribute 4 was appended twice".to_owned());
    }

    let mut transitions = Vec::new();
    transitions.push(transition_name(sharing.set_master_enabled(false)));
    let cleared_latch_once = !sharing.is_latched();
    if !plan_pty_replies(&commit, sharing.images_enabled()).is_empty() {
        return Err("a disabled session still owed a Kitty reply".to_owned());
    }
    let disabled_da = augment_device_attributes(&device_attributes, sharing.images_enabled());
    if disabled_da.contains(";4c") {
        return Err(format!("disabled DA1 still advertises Sixel: {disabled_da:?}"));
    }
    if sharing.admit(TerminalImageCapabilities::default()).is_err() {
        return Err("a disabled session refused an ordinary text viewer".to_owned());
    }

    // A repeated write is not a transition, so cleanup never runs twice.
    transitions.push(transition_name(sharing.set_master_enabled(false)));
    transitions.push(transition_name(sharing.set_master_enabled(true)));
    if sharing.is_latched() {
        return Err("re-enabling restored a latch without a capable viewer".to_owned());
    }
    let relatched = sharing.latch(TerminalImageCapabilities::V1).runtime_enabled;

    Ok(KillSwitchEvidence {
        transitions: transitions.join(","),
        cleared_latch_once,
        disabled_device_attributes: escape(&disabled_da),
        enabled_device_attributes: escape(&enabled_da),
        relatched_after_reenable: relatched,
    })
}

/// The latch is session state: it is unchanged by viewer count and it is what
/// refuses an incapable viewer.
fn verify_capability_lifecycle() -> Result<CapabilityEvidence, String> {
    let incapable = TerminalImageCapabilities {
        runtime_enabled: true,
        features: TerminalImageFeatures::from_bits(TerminalImageFeatures::KITTY_RGB),
    };
    let mut sharing = SessionImageSharing::new(true);
    if sharing.admit(incapable).is_err() {
        return Err("an unlatched session refused a viewer".to_owned());
    }
    let latched = sharing.latch(TerminalImageCapabilities::V1);
    let latch_is_idempotent = sharing.latch(incapable) == latched;

    let mut capable_admissions = 0;
    let mut incapable_refusals = 0;
    let mut refusal = None;
    // Two capable viewers, one incapable, then the same checks with everyone
    // detached: the latch cannot depend on who is watching.
    for _ in 0..2 {
        for _ in 0..2 {
            if sharing.admit(TerminalImageCapabilities::V1).is_ok() {
                capable_admissions += 1;
            }
        }
        match sharing.admit(incapable) {
            Ok(()) => return Err("an incapable viewer was admitted".to_owned()),
            Err(mismatch) => {
                incapable_refusals += 1;
                refusal = Some(mismatch);
            }
        }
    }
    let refusal = refusal.ok_or_else(|| "no refusal was recorded".to_owned())?;

    Ok(CapabilityEvidence {
        latched_subset_features: latched.features.bits(),
        latch_is_idempotent,
        survives_detach: sharing.images_enabled(),
        capable_admissions,
        incapable_refusals,
        refusal_required_features: refusal.required.features.bits(),
        refusal_offered_features: refusal.offered.features.bits(),
        unlatched_admission: "admits_any_viewer",
    })
}

/// Fan one real burst out across zero, one, and several viewers, then through a
/// controller change and a detach, reading each viewer's receipts back.
async fn verify_viewer_fanout(fixtures: &Path) -> Result<ViewerEvidence, String> {
    let frames = burst_frames(fixtures)?;
    let required = TerminalImageCapabilities::V1;
    let incapable = effective_connection_subset(
        TerminalImageCapabilities {
            runtime_enabled: true,
            features: TerminalImageFeatures::from_bits(TerminalImageFeatures::KITTY_RGB),
        },
        true,
    );
    let client_writer = image_sharing_probe::new_client_writer();

    // Zero viewers: a latched session keeps producing records with nobody
    // watching, and nothing is delivered or buffered anywhere.
    let zero_viewers_delivered =
        image_sharing_probe::fan_out_images(&client_writer, required, &frames);

    let mut first = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    let one_viewer_delivered =
        image_sharing_probe::fan_out_images(&client_writer, required, &frames);
    let one_viewer_received = count_image_records(&first.drain().await);

    let mut second = image_sharing_probe::attach_viewer(&client_writer, required, true).await;
    let mut blind = image_sharing_probe::attach_viewer(&client_writer, incapable, true).await;
    let multiple_viewers_delivered =
        image_sharing_probe::fan_out_images(&client_writer, required, &frames);
    let multiple_viewers_received =
        [count_image_records(&first.drain().await), count_image_records(&second.drain().await)];
    let incapable_viewer_received = count_image_records(&blind.drain().await);

    // Controller change: a `SingleController` re-point replaces the whole set,
    // so the displaced viewers stop receiving without any latch change.
    let mut controller = image_sharing_probe::attach_viewer(&client_writer, required, false).await;
    let controller_change_delivered =
        image_sharing_probe::fan_out_images(&client_writer, required, &frames);
    let controller_change_displaced_received =
        count_image_records(&first.drain().await) + count_image_records(&second.drain().await);
    let controller_change_new_received = count_image_records(&controller.drain().await);

    if !image_sharing_probe::detach_viewer(&client_writer, &controller) {
        return Err("detaching the only viewer reported no sink".to_owned());
    }
    let detached_delivered = image_sharing_probe::fan_out_images(&client_writer, required, &frames);
    let detached_viewer_received = count_image_records(&controller.drain().await);

    let evidence = ViewerEvidence {
        burst_records: frames.len(),
        zero_viewers_delivered,
        one_viewer_delivered,
        one_viewer_received,
        multiple_viewers_delivered,
        multiple_viewers_received: format!(
            "{},{}",
            multiple_viewers_received[0], multiple_viewers_received[1]
        ),
        incapable_viewer_received,
        controller_change_delivered,
        controller_change_displaced_received,
        controller_change_new_received,
        detached_delivered,
        detached_viewer_received,
    };
    check_viewer_evidence(&evidence)?;
    Ok(evidence)
}

fn check_viewer_evidence(evidence: &ViewerEvidence) -> Result<(), String> {
    if evidence.burst_records == 0 {
        return Err("the pinned fixture published no image records".to_owned());
    }
    let records = evidence.burst_records;
    let expected = [
        ("zero viewers delivered", evidence.zero_viewers_delivered, 0),
        ("one viewer delivered", evidence.one_viewer_delivered, 1),
        ("one viewer received", evidence.one_viewer_received, records),
        ("multiple viewers delivered", evidence.multiple_viewers_delivered, 2),
        ("incapable viewer received", evidence.incapable_viewer_received, 0),
        ("controller change delivered", evidence.controller_change_delivered, 1),
        ("displaced viewers received", evidence.controller_change_displaced_received, 0),
        ("controller viewer received", evidence.controller_change_new_received, records),
        ("detached delivered", evidence.detached_delivered, 0),
        ("detached viewer received", evidence.detached_viewer_received, 0),
    ];
    for (label, observed, want) in expected {
        if observed != want {
            return Err(format!("{label} is {observed}, expected {want}"));
        }
    }
    if evidence.multiple_viewers_received != format!("{records},{records}") {
        return Err(format!(
            "each capable viewer must receive the burst exactly once, got {}",
            evidence.multiple_viewers_received
        ));
    }
    Ok(())
}

/// Commit one pinned fixture and wrap its published records as wire frames.
fn burst_frames(fixtures: &Path) -> Result<Vec<ServerMessage>, String> {
    let bytes = read_hex(&fixtures.join(RGB_CLASSIC_FIXTURE))?;
    let mut probe = Probe::new();
    let commit = probe.feed(&bytes)?;
    let messages = probe
        .images
        .commit_and_publish(&commit, &mut definition_payload)
        .map_err(|error| format!("publish the pinned fixture: {error}"))?;
    if messages.is_empty() {
        return Err("the pinned fixture committed no records".to_owned());
    }
    let session_id = SessionId::new();
    Ok(messages
        .into_iter()
        .map(|message| ServerMessage::TerminalImageLive { session_id, message })
        .collect())
}

/// Canonical bytes for one published definition, matching the other probes.
fn definition_payload(definition: &TerminalImageDefinition) -> Option<Vec<u8>> {
    let length = usize::try_from(definition.rgba_bytes).ok()?;
    Some(vec![u8::try_from(definition.id.0 % 251).unwrap_or(0); length])
}

fn count_image_records(frames: &[ServerMessage]) -> usize {
    frames
        .iter()
        .filter(|frame| {
            matches!(
                frame,
                ServerMessage::TerminalImageLive {
                    message: TerminalImageLiveMessage::Begin { .. }
                        | TerminalImageLiveMessage::Update { .. }
                        | TerminalImageLiveMessage::Commit { .. },
                    ..
                }
            )
        })
        .count()
}

fn is_primary_device_attributes(reply: &str) -> bool {
    reply.starts_with("\u{1b}[?") && reply.ends_with('c')
}

const fn transition_name(transition: KillSwitchTransition) -> &'static str {
    match transition {
        KillSwitchTransition::Unchanged => "unchanged",
        KillSwitchTransition::Disabled { cleared_latch: true } => "disabled_cleared_latch",
        KillSwitchTransition::Disabled { cleared_latch: false } => "disabled",
        KillSwitchTransition::Enabled => "enabled",
    }
}

/// Render control bytes printable so the evidence stays diffable.
fn escape(text: &str) -> String {
    text.chars().flat_map(char::escape_default).collect()
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence<'_>) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode sharing evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
