//! Production-path evidence for retiring incomplete graphics transfers.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use scribe_image_decode::{
    DecodeCeilings, DecodePermit, DecodeRequest, DecodeScheduler, DecodeSchedulerMetrics,
    DecodeSessionId, DecodeTarget,
};
use scribe_pty::graphics_framing::{GraphicsFailureCategory, GraphicsProtocol};
use scribe_server::terminal_image_state::{
    ImageStorageOwnership, PtyTerminalImageState, SessionTerminalCommit, SessionTerminalOutput,
    TerminalImageBoundary, TerminalImageProcessPolicy, TransferRetirement,
};
use serde::Serialize;

use crate::decode_storage::decode_storage;

/// Payload-free boolean claim, kept as its own type so evidence structs stay
/// readable rather than becoming bare boolean tuples.
#[derive(Serialize)]
#[serde(transparent)]
struct EvidenceCheck(bool);

impl From<bool> for EvidenceCheck {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

#[derive(Serialize)]
struct Evidence {
    schema_version: u32,
    status: &'static str,
    engine: &'static str,
    retirement: RetirementEvidence,
    invisibility: InvisibilityEvidence,
    idempotence: IdempotenceEvidence,
    admission: AdmissionEvidence,
    chronology: ChronologyEvidence,
    ownership: OwnershipEvidence,
}

/// The typed boundary each abandoned transfer retired with.
#[derive(Serialize)]
struct RetirementEvidence {
    partial_apc: &'static str,
    partial_apc_protocol: &'static str,
    partial_dcs: &'static str,
    partial_dcs_protocol: &'static str,
    split_terminator: &'static str,
    kitty_chunks: &'static str,
    compressed_chunks: &'static str,
    reset: &'static str,
    close: &'static str,
    candidate_text_retired_as: &'static str,
}

/// No incomplete transfer became visible canonical state.
#[derive(Serialize)]
struct InvisibilityEvidence {
    published_images: usize,
    definitions: usize,
    placements: usize,
    generation_unchanged: EvidenceCheck,
}

/// Repeating a retirement changes nothing and cannot underflow accounting.
#[derive(Serialize)]
struct IdempotenceEvidence {
    repeated_reset_outputs: usize,
    repeated_close_outputs: usize,
    counters_stable: EvidenceCheck,
    ledger_healthy: EvidenceCheck,
}

/// Cancellation and queue-wait deadlines retire the transfer they refuse.
#[derive(Serialize)]
struct AdmissionEvidence {
    cancelled: &'static str,
    cancelled_pending_cleared: EvidenceCheck,
    cancelled_count: u64,
    deadline: &'static str,
    deadline_pending_cleared: EvidenceCheck,
    expired_count: u64,
    close_cancelled_waiter: EvidenceCheck,
}

/// Query replies keep their FIFO position ahead of a later retirement.
#[derive(Serialize)]
struct ChronologyEvidence {
    reply_ids: Vec<u64>,
    reply_sequences: Vec<u64>,
    retirement_sequence: u64,
    fifo: EvidenceCheck,
}

/// Every retirement path released storage and admissions exactly once.
#[derive(Serialize)]
struct OwnershipEvidence {
    cases: usize,
    pending_after_cases: usize,
    retained_bytes_after_cases: usize,
    session_requested_current: u64,
    process_requested_current: u64,
    admitted: u64,
    released: u64,
    queued: u32,
    active: u32,
}

/// One complete Kitty RGBA command, optionally a non-final transfer chunk.
fn kitty(controls: &str, rgba: &[u8]) -> Vec<u8> {
    format!("\x1b_G{controls};{}\x1b\\", STANDARD.encode(rgba)).into_bytes()
}

/// A Kitty transmit whose APC string never terminates.
const PARTIAL_APC: &[u8] = b"\x1b_Gf=32,s=1,v=1;AAAAAA";
/// A Sixel DCS whose string never terminates.
const PARTIAL_DCS: &[u8] = b"\x1bPq~~~";
/// A Kitty transmit cut between the two bytes of its ST terminator.
const SPLIT_TERMINATOR: &[u8] = b"\x1b_Gf=32,s=1,v=1;AAAAAA\x1b";
/// Ordinary text ending on a bare escape the framer is still classifying.
const CANDIDATE_TEXT: &[u8] = b"hello\x1b";

fn category_name(category: GraphicsFailureCategory) -> &'static str {
    match category {
        GraphicsFailureCategory::UnsupportedProtocol => "unsupported_protocol",
        GraphicsFailureCategory::UnsupportedAction => "unsupported_action",
        GraphicsFailureCategory::UnsupportedTransport => "unsupported_transport",
        GraphicsFailureCategory::MalformedFraming => "malformed_framing",
        GraphicsFailureCategory::MalformedControl => "malformed_control",
        GraphicsFailureCategory::MalformedPayload => "malformed_payload",
        GraphicsFailureCategory::TruncatedSequence => "truncated_sequence",
        GraphicsFailureCategory::QuotaExceeded => "quota_exceeded",
    }
}

fn protocol_name(protocol: GraphicsProtocol) -> &'static str {
    match protocol {
        GraphicsProtocol::Kitty => "kitty",
        GraphicsProtocol::Sixel => "sixel",
    }
}

fn retained_bytes(ownership: ImageStorageOwnership) -> usize {
    ownership.pending_kitty_requested
        + ownership.completed_kitty_requested
        + ownership.sixel_body_requested
        + ownership.kitty_decoded_requested
        + ownership.sixel_decoded_requested
}

/// Count boundaries that published a decoded image.
fn published_images(commit: &SessionTerminalCommit) -> usize {
    commit
        .outputs
        .as_slice()
        .iter()
        .filter(|output| {
            matches!(
                output,
                SessionTerminalOutput::Image {
                    boundary: TerminalImageBoundary::Kitty { decoded: Some(_), .. }
                        | TerminalImageBoundary::Sixel { .. },
                    ..
                }
            )
        })
        .count()
}

/// Ordered typed failure boundaries with their protocol and sequence.
fn failures(commit: &SessionTerminalCommit) -> Vec<(&'static str, &'static str, u64)> {
    commit
        .outputs
        .as_slice()
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Image {
                sequence,
                boundary: TerminalImageBoundary::Failure(failure),
                ..
            } => {
                Some((category_name(failure.category), protocol_name(failure.protocol), sequence.0))
            }
            _ => None,
        })
        .collect()
}

fn raw_bytes(commit: &SessionTerminalCommit) -> usize {
    commit
        .outputs
        .as_slice()
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Raw(raw) => Some(raw.len()),
            SessionTerminalOutput::Image { .. } => None,
        })
        .sum()
}

/// What one retirement path proved, payload-free.
struct CaseOutcome {
    failures: Vec<(&'static str, &'static str, u64)>,
    /// Every raw byte the whole case delivered to the ordinary terminal.
    raw_bytes: usize,
    published_images: usize,
    definitions: usize,
    placements: usize,
    pending_after: usize,
    retained_bytes: usize,
    session_requested_current: u64,
    process_requested_current: u64,
    generation_unchanged: bool,
}

impl CaseOutcome {
    fn category(&self) -> &'static str {
        self.failures.first().map_or("none", |(category, _, _)| *category)
    }

    fn protocol(&self) -> &'static str {
        self.failures.first().map_or("none", |(_, protocol, _)| *protocol)
    }
}

/// Feed a hostile stream through production framing up to an incomplete
/// transfer, retire it, and report the typed outcome plus exact ownership.
fn retire_case(reads: &[&[u8]], retirement: TransferRetirement) -> Result<CaseOutcome, String> {
    let mut session = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    let generation_before = session.state().generation;
    let mut published = 0;
    let mut raw_total = 0;
    for read in reads {
        let commit = session.process_bytes(read).map_err(|error| error.to_string())?;
        published += published_images(&commit);
        raw_total += raw_bytes(&commit);
        session.commit_mutations(&commit).map_err(|error| error.to_string())?;
    }
    let commit =
        session.retire_transfers(retirement).map_err(|error| format!("retire: {error}"))?;
    published += published_images(&commit);
    session.commit_mutations(&commit).map_err(|error| format!("retire mutations: {error}"))?;
    let failures = failures(&commit);
    raw_total += raw_bytes(&commit);
    drop(commit);
    let state = session.state();
    let pending_after = usize::from(state.pending_transfer.is_some())
        + usize::from(session.validation_pending_kitty_decode_state().is_some());
    let (process, session_counters) =
        session.storage_counters().map_err(|error| format!("counters: {error}"))?;
    Ok(CaseOutcome {
        failures,
        raw_bytes: raw_total,
        published_images: published,
        definitions: session.canonical_definitions().len(),
        placements: session.canonical_placements().len(),
        pending_after,
        retained_bytes: retained_bytes(session.storage_ownership()),
        session_requested_current: session_counters.requested_current,
        process_requested_current: process.requested_current,
        generation_unchanged: state.generation == generation_before,
    })
}

/// Every EOF, reset, and close path retires its incomplete transfer with a
/// protocol-correct typed boundary and no surviving pending metadata.
// @lat: [[test#Test Harness#Incomplete Transfer Retirement#Stream Termination Paths]]
fn verify_retirement()
-> Result<(RetirementEvidence, InvisibilityEvidence, OwnershipEvidence), String> {
    // One 1x1 RGBA transfer split so the first chunk is never followed by its
    // final chunk, in raw and zlib transports. A non-final chunk cannot carry
    // base64 padding, so each payload is a whole three-byte group.
    let chunk = kitty("f=32,s=1,v=1,i=7,m=1", &[1, 2, 3]);
    let compressed_chunk = kitty("f=32,o=z,s=1,v=1,i=8,m=1", &[0x78, 0x9c, 0x01]);
    let partial_apc = retire_case(&[PARTIAL_APC], TransferRetirement::StreamEnd)?;
    let partial_dcs = retire_case(&[PARTIAL_DCS], TransferRetirement::StreamEnd)?;
    let split_terminator = retire_case(&[SPLIT_TERMINATOR], TransferRetirement::StreamEnd)?;
    let candidate_text = retire_case(&[CANDIDATE_TEXT], TransferRetirement::StreamEnd)?;
    let kitty_chunks = retire_case(&[&chunk], TransferRetirement::Reset)?;
    let compressed_chunks = retire_case(&[&compressed_chunk], TransferRetirement::Close)?;
    let reset = retire_case(&[PARTIAL_APC], TransferRetirement::Reset)?;
    let close = retire_case(&[PARTIAL_DCS], TransferRetirement::Close)?;
    // A payload split across two reads is still one incomplete transfer.
    let split_payload =
        retire_case(&[b"\x1b_Gf=32,s=1,v=1;AA", b"AAAA"], TransferRetirement::StreamEnd)?;
    let cases = [
        ("partial_apc", &partial_apc),
        ("partial_dcs", &partial_dcs),
        ("split_terminator", &split_terminator),
        ("candidate_text", &candidate_text),
        ("kitty_chunks", &kitty_chunks),
        ("compressed_chunks", &compressed_chunks),
        ("reset", &reset),
        ("close", &close),
        ("split_payload", &split_payload),
    ];

    for (name, outcome) in cases {
        if outcome.pending_after != 0 {
            return Err(format!("{name} left {} pending transfers", outcome.pending_after));
        }
        if outcome.retained_bytes != 0
            || outcome.session_requested_current != 0
            || outcome.process_requested_current != 0
        {
            return Err(format!("{name} retained storage after retirement"));
        }
        if outcome.published_images != 0 || outcome.definitions != 0 || outcome.placements != 0 {
            return Err(format!("{name} published incomplete content"));
        }
        if !outcome.generation_unchanged {
            return Err(format!("{name} consumed a generation"));
        }
    }

    if candidate_text.raw_bytes != CANDIDATE_TEXT.len() {
        return Err("stream end did not flush candidate text as raw bytes".to_owned());
    }
    if !reset.failures.is_empty() || !close.failures.is_empty() {
        return Err("reset or close published a boundary for discarded framing".to_owned());
    }

    let retirement = RetirementEvidence {
        partial_apc: partial_apc.category(),
        partial_apc_protocol: partial_apc.protocol(),
        partial_dcs: partial_dcs.category(),
        partial_dcs_protocol: partial_dcs.protocol(),
        split_terminator: split_terminator.category(),
        kitty_chunks: kitty_chunks.category(),
        compressed_chunks: compressed_chunks.category(),
        reset: "discarded",
        close: "discarded",
        candidate_text_retired_as: "raw",
    };
    let invisibility = InvisibilityEvidence {
        published_images: cases.iter().map(|(_, outcome)| outcome.published_images).sum(),
        definitions: cases.iter().map(|(_, outcome)| outcome.definitions).sum(),
        placements: cases.iter().map(|(_, outcome)| outcome.placements).sum(),
        generation_unchanged: cases.iter().all(|(_, outcome)| outcome.generation_unchanged).into(),
    };
    let ownership = OwnershipEvidence {
        cases: cases.len(),
        pending_after_cases: cases.iter().map(|(_, outcome)| outcome.pending_after).sum(),
        retained_bytes_after_cases: cases.iter().map(|(_, outcome)| outcome.retained_bytes).sum(),
        session_requested_current: 0,
        process_requested_current: 0,
        admitted: 0,
        released: 0,
        queued: 0,
        active: 0,
    };
    Ok((retirement, invisibility, ownership))
}

/// Retiring an already-retired session is a no-op that cannot underflow the
/// storage ledger.
// @lat: [[test#Test Harness#Incomplete Transfer Retirement#Idempotent Repetition]]
fn verify_idempotence() -> Result<IdempotenceEvidence, String> {
    let mut session = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    session.process_bytes(PARTIAL_APC).map_err(|error| error.to_string())?;
    for retirement in [TransferRetirement::Reset, TransferRetirement::Close] {
        session.retire_transfers(retirement).map_err(|error| format!("first: {error}"))?;
    }
    let (process_before, session_before) =
        session.storage_counters().map_err(|error| format!("counters: {error}"))?;
    let repeated_reset_outputs = session
        .retire_transfers(TransferRetirement::Reset)
        .map_err(|error| format!("repeat reset: {error}"))?
        .outputs
        .as_slice()
        .len();
    let repeated_close_outputs = session
        .retire_transfers(TransferRetirement::Close)
        .map_err(|error| format!("repeat close: {error}"))?
        .outputs
        .as_slice()
        .len();
    let (process_after, session_after) =
        session.storage_counters().map_err(|error| format!("counters: {error}"))?;
    Ok(IdempotenceEvidence {
        repeated_reset_outputs,
        repeated_close_outputs,
        counters_stable: (process_before.requested_current == process_after.requested_current
            && session_before.requested_current == session_after.requested_current)
            .into(),
        ledger_healthy: (process_after.requested_current == 0
            && session_after.requested_current == 0)
            .into(),
    })
}

fn ceilings(wait: Duration) -> DecodeCeilings {
    DecodeCeilings { concurrent_decodes: 1, queue_depth: 4, queue_bytes: 1 << 20, queue_wait: wait }
}

/// Occupy the single decode slot from a session unrelated to the seam.
fn occupy_slot(scheduler: &Arc<DecodeScheduler>) -> Result<DecodePermit, String> {
    let ticket = scheduler
        .issue(DecodeRequest {
            session: scheduler.new_session(),
            generation: 1,
            target: DecodeTarget::kitty(9001),
            requested_bytes: 4,
            storage: decode_storage(),
        })
        .map_err(|error| format!("hold issue: {error}"))?;
    scheduler.admit(ticket).map_err(|error| format!("hold admit: {error}"))
}

/// Block until the scheduler reports at least one queued waiter.
fn await_queued(scheduler: &Arc<DecodeScheduler>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let metrics = scheduler.metrics().map_err(|error| format!("metrics: {error}"))?;
        if metrics.queued > 0 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err("no decode waiter appeared".to_owned())
}

/// What one read whose decode admission was refused left behind.
struct RefusedRead {
    category: &'static str,
    published: usize,
    pending: usize,
    retained: usize,
    session_current: u64,
    process_current: u64,
}

/// Drive one read and report its typed boundary plus surviving ownership.
fn refused_read(session: &mut PtyTerminalImageState, chunk: &[u8]) -> Result<RefusedRead, String> {
    let commit = session.process_bytes(chunk).map_err(|error| error.to_string())?;
    let category = failures(&commit).first().map_or("none", |(category, _, _)| *category);
    let published = published_images(&commit);
    drop(commit);
    let pending = usize::from(session.state().pending_transfer.is_some())
        + usize::from(session.validation_pending_kitty_decode_state().is_some());
    let (process, session_counters) =
        session.storage_counters().map_err(|error| format!("counters: {error}"))?;
    Ok(RefusedRead {
        category,
        published,
        pending,
        retained: retained_bytes(session.storage_ownership()),
        session_current: session_counters.requested_current,
        process_current: process.requested_current,
    })
}

/// Feed a non-final chunk from a thread that will block in admission, then
/// cancel it from this thread once the scheduler shows the waiter.
fn cancel_queued_read(
    chunk: &[u8],
    cancel: impl FnOnce(&Arc<DecodeScheduler>, DecodeSessionId) -> Result<usize, String>,
) -> Result<(RefusedRead, usize), String> {
    let policy = TerminalImageProcessPolicy::with_decode_ceilings_for_validation(ceilings(
        Duration::from_secs(10),
    ));
    let scheduler = Arc::clone(policy.decode_scheduler());
    let mut session = PtyTerminalImageState::new(policy);
    let decode_session = session.decode_session();
    let holder = occupy_slot(&scheduler)?;
    let feeding = {
        let chunk = chunk.to_vec();
        thread::spawn(move || refused_read(&mut session, &chunk))
    };
    await_queued(&scheduler)?;
    let cancelled = cancel(&scheduler, decode_session)?;
    let outcome = feeding.join().map_err(|_| "cancellation probe panicked".to_owned())??;
    drop(holder);
    if outcome.published != 0 || outcome.retained != 0 {
        return Err("a cancelled transfer published or retained content".to_owned());
    }
    Ok((outcome, cancelled))
}

/// Let one seam's own admission outlive its queue wait behind a held slot.
fn expire_read(chunk: &[u8]) -> Result<(RefusedRead, DecodeSchedulerMetrics), String> {
    let policy = TerminalImageProcessPolicy::with_decode_ceilings_for_validation(ceilings(
        Duration::from_millis(120),
    ));
    let scheduler = Arc::clone(policy.decode_scheduler());
    let mut session = PtyTerminalImageState::new(policy);
    let holder = occupy_slot(&scheduler)?;
    let outcome = refused_read(&mut session, chunk)?;
    drop(holder);
    if outcome.published != 0 || outcome.retained != 0 {
        return Err("an expired transfer published or retained content".to_owned());
    }
    let metrics = scheduler.metrics().map_err(|error| format!("deadline metrics: {error}"))?;
    Ok((outcome, metrics))
}

/// A refused admission - cancelled or timed out - retires the transfer that
/// requested it instead of leaving buffered chunks behind.
// @lat: [[test#Test Harness#Incomplete Transfer Retirement#Refused Admission Paths]]
fn verify_admission() -> Result<(AdmissionEvidence, OwnershipEvidence), String> {
    let chunk = kitty("f=32,s=1,v=1,i=7,m=1", &[1, 2, 3]);
    let (cancelled, cancelled_count) = cancel_queued_read(&chunk, |scheduler, session| {
        scheduler
            .cancel_target(session, DecodeTarget::kitty(7))
            .map_err(|error| format!("cancel target: {error}"))
    })?;
    // Close cancels every admission the closing session still owns; this is the
    // exact call `retire_transfers(Close)` makes.
    let (closed, close_cancelled) = cancel_queued_read(&chunk, |scheduler, session| {
        scheduler.cancel_session(session).map_err(|error| format!("cancel session: {error}"))
    })?;
    let (expired, metrics) = expire_read(&chunk)?;

    Ok((
        AdmissionEvidence {
            cancelled: cancelled.category,
            cancelled_pending_cleared: (cancelled.pending == 0).into(),
            cancelled_count: u64::try_from(cancelled_count).unwrap_or(u64::MAX),
            deadline: expired.category,
            deadline_pending_cleared: (expired.pending == 0).into(),
            expired_count: metrics.expired,
            close_cancelled_waiter: (close_cancelled == 1
                && closed.category == "quota_exceeded"
                && closed.pending == 0)
                .into(),
        },
        OwnershipEvidence {
            cases: 3,
            pending_after_cases: cancelled.pending + closed.pending + expired.pending,
            retained_bytes_after_cases: cancelled.retained + closed.retained + expired.retained,
            session_requested_current: expired.session_current,
            process_requested_current: expired.process_current,
            admitted: metrics.admitted,
            released: metrics.released,
            queued: metrics.queued,
            active: metrics.active,
        },
    ))
}

/// Kitty query replies stay in issue order and keep their FIFO position ahead
/// of the boundary that retires a later incomplete transfer.
// @lat: [[test#Test Harness#Incomplete Transfer Retirement#Reply Chronology]]
fn verify_chronology() -> Result<ChronologyEvidence, String> {
    let mut session = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    let mut read = kitty("a=q,i=31,f=32,s=1,v=1", &[1, 2, 3, 4]);
    read.extend_from_slice(&kitty("a=q,i=32,f=32,s=1,v=1", &[5, 6, 7, 8]));
    read.extend_from_slice(PARTIAL_APC);
    let commit = session.process_bytes(&read).map_err(|error| error.to_string())?;
    let replies: Vec<(u64, u64)> = commit
        .outputs
        .as_slice()
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Image {
                sequence,
                boundary: TerminalImageBoundary::Kitty { command, .. },
                ..
            } => command.controls().image_id.map(|id| (u64::from(id), sequence.0)),
            _ => None,
        })
        .collect();
    drop(commit);
    let retirement = session
        .retire_transfers(TransferRetirement::StreamEnd)
        .map_err(|error| format!("retire: {error}"))?;
    let retirement_sequence = failures(&retirement).first().map_or(0, |(_, _, sequence)| *sequence);
    drop(retirement);

    let reply_ids: Vec<u64> = replies.iter().map(|(id, _)| *id).collect();
    let reply_sequences: Vec<u64> = replies.iter().map(|(_, sequence)| *sequence).collect();
    let fifo = reply_ids == vec![31, 32]
        && reply_sequences.is_sorted_by(|left, right| left < right)
        && reply_sequences.iter().all(|sequence| *sequence < retirement_sequence);
    Ok(ChronologyEvidence { reply_ids, reply_sequences, retirement_sequence, fifo: fifo.into() })
}

/// Run every incomplete-transfer retirement probe and write payload-free
/// evidence for the functional gate.
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let (retirement, invisibility, retirement_ownership) = verify_retirement()?;
    let idempotence = verify_idempotence()?;
    let (admission, admission_ownership) = verify_admission()?;
    let chronology = verify_chronology()?;

    let ownership = OwnershipEvidence {
        cases: retirement_ownership.cases + admission_ownership.cases,
        pending_after_cases: retirement_ownership.pending_after_cases
            + admission_ownership.pending_after_cases,
        retained_bytes_after_cases: retirement_ownership.retained_bytes_after_cases
            + admission_ownership.retained_bytes_after_cases,
        ..admission_ownership
    };

    let failures = [
        (retirement.partial_apc != "truncated_sequence", "a partial APC frame was not truncated"),
        (retirement.partial_apc_protocol != "kitty", "a partial APC frame lost its protocol"),
        (retirement.partial_dcs != "truncated_sequence", "a partial DCS frame was not truncated"),
        (retirement.partial_dcs_protocol != "sixel", "a partial DCS frame lost its protocol"),
        (retirement.split_terminator != "truncated_sequence", "a split terminator survived EOF"),
        (retirement.kitty_chunks != "truncated_sequence", "incomplete Kitty chunks survived"),
        (
            retirement.compressed_chunks != "truncated_sequence",
            "incomplete compressed chunks survived",
        ),
        (invisibility.published_images != 0, "an incomplete transfer published an image"),
        (invisibility.definitions != 0, "an incomplete transfer defined an image"),
        (invisibility.placements != 0, "an incomplete transfer placed an image"),
        (!invisibility.generation_unchanged.0, "an incomplete transfer consumed a generation"),
        (idempotence.repeated_reset_outputs != 0, "a repeated reset produced output"),
        (idempotence.repeated_close_outputs != 0, "a repeated close produced output"),
        (!idempotence.counters_stable.0, "a repeated retirement moved the ledger"),
        (!idempotence.ledger_healthy.0, "a repeated retirement underflowed the ledger"),
        (admission.cancelled != "quota_exceeded", "a cancelled admission had no typed boundary"),
        (!admission.cancelled_pending_cleared.0, "a cancelled admission left pending state"),
        (admission.cancelled_count != 1, "cancellation reached the wrong number of entries"),
        (admission.deadline != "quota_exceeded", "an expired admission had no typed boundary"),
        (!admission.deadline_pending_cleared.0, "an expired admission left pending state"),
        (admission.expired_count == 0, "a queue-wait deadline was not counted"),
        (!admission.close_cancelled_waiter.0, "close did not cancel its own queued admission"),
        (!chronology.fifo.0, "query replies lost FIFO chronology"),
        (ownership.pending_after_cases != 0, "a retirement path left pending metadata"),
        (ownership.retained_bytes_after_cases != 0, "a retirement path retained storage"),
        (
            ownership.session_requested_current != 0 || ownership.process_requested_current != 0,
            "a retirement path leaked storage",
        ),
        (ownership.admitted != ownership.released, "an admission was not released"),
        (ownership.queued != 0 || ownership.active != 0, "scheduler state survived retirement"),
    ];
    if let Some((_, reason)) = failures.iter().find(|(failed, _)| *failed) {
        return Err((*reason).to_owned());
    }

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "production session terminal seam",
        retirement,
        invisibility,
        idempotence,
        admission,
        chronology,
        ownership,
    };
    write_evidence(evidence_path, &evidence)
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode transfer lifecycle evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
