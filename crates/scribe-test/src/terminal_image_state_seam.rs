//! Functional evidence for the production server-owned terminal image seam.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use scribe_common::terminal_images::{ImageLimits, TerminalOutputSequence, TerminalScreenKind};
use scribe_pty::graphics_framing::{GraphicsFailureCategory, GraphicsProtocol, SixelMode};
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, SessionTerminalCommit, SessionTerminalError, SessionTerminalOutput,
    TerminalImageBoundary, TerminalImageProcessPolicy, process_pty_reader_ingress,
};
use serde::Serialize;

#[derive(Serialize)]
struct Evidence<'a> {
    schema_version: u32,
    status: &'a str,
    engine: &'a str,
    process_policy: ProcessPolicyEvidence,
    ordered_boundaries: Vec<&'a str>,
    pending_transfer: PendingTransferEvidence,
    framing_work: FramingWorkEvidence,
    transactional_exhaustion: TransactionalExhaustionEvidence,
    state_ownership: StateOwnership,
    routing: RoutingEvidence,
    live_image_fanout: &'a str,
    legacy_messagepack_bytes: &'a str,
}

#[derive(Serialize)]
struct ProcessPolicyEvidence {
    shared: bool,
    immutable_v1_limits: bool,
}

#[derive(Serialize)]
struct PendingTransferEvidence {
    payload_free: bool,
}

#[derive(Serialize)]
struct TransactionalExhaustionEvidence {
    typed_rejection: &'static str,
    state_unchanged: &'static str,
    offset_unconsumed: &'static str,
    speculative_clone_reads: u64,
}

#[derive(Serialize)]
struct FramingWorkEvidence {
    large_transfer_bytes: usize,
    split_read_bytes: usize,
    direct_reads: u64,
    speculative_clone_reads: u64,
    direct_edge_cases: &'static str,
    client_delivery_calls: u64,
    term_feed_calls: u64,
    matching_digest: bool,
}

#[derive(Serialize)]
struct RoutingEvidence {
    client_delivery_calls: u64,
    term_feed_calls: u64,
    client_bytes: u64,
    term_bytes: u64,
    matching_digest: bool,
}

#[derive(Serialize)]
struct StateOwnership {
    generation: u64,
    sequence: u64,
    active_screen: &'static str,
    definition_count: usize,
    placement_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservedSink {
    calls: u64,
    bytes: u64,
    digest: u64,
}

impl ObservedSink {
    fn observe(&mut self, bytes: &[u8]) {
        let calls = self.calls.checked_add(1);
        assert!(calls.is_some(), "controlled sink calls overflowed");
        self.calls = calls.unwrap_or(self.calls);
        let byte_len = u64::try_from(bytes.len());
        assert!(byte_len.is_ok(), "controlled sink input length overflowed");
        let observed_bytes = self.bytes.checked_add(byte_len.unwrap_or(0));
        assert!(observed_bytes.is_some(), "controlled sink bytes overflowed");
        self.bytes = observed_bytes.unwrap_or(self.bytes);
        self.digest = bytes.iter().fold(self.digest, |digest, byte| {
            digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
        });
    }
}

// @lat: [[test#Test Harness#Terminal Image Session State Seam#Production Seam Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create seam probe runtime: {error}"))?;
    runtime.block_on(run_probe(evidence_path))
}

async fn run_probe(evidence_path: &Path) -> Result<(), String> {
    let (mut session, policy) = shared_session()?;
    let client = Rc::new(RefCell::new(ObservedSink::default()));
    let term = Rc::new(RefCell::new(ObservedSink::default()));
    verify_pending_transfer(&mut session, &client, &term).await?;
    let order = verify_ordered_completion(&mut session, &client, &term).await?;
    let state_ownership = verify_owned_state(&mut session)?;
    verify_direct_edge_cases().await?;
    let framing_work = verify_large_split_transfer().await?;
    let transactional_exhaustion = verify_transactional_exhaustion().await?;
    let client_observed = *client.borrow();
    let term_observed = *term.borrow();
    if client_observed != term_observed || client_observed.calls != 2 {
        return Err(format!(
            "shared ingress did not route ordinary bytes once per read: client={client_observed:?} term={term_observed:?}"
        ));
    }

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server process_pty_reader_ingress",
        process_policy: ProcessPolicyEvidence { shared: true, immutable_v1_limits: true },
        ordered_boundaries: order,
        pending_transfer: PendingTransferEvidence { payload_free: true },
        framing_work,
        transactional_exhaustion,
        state_ownership,
        routing: RoutingEvidence {
            client_delivery_calls: client_observed.calls,
            term_feed_calls: term_observed.calls,
            client_bytes: client_observed.bytes,
            term_bytes: term_observed.bytes,
            matching_digest: client_observed.digest == term_observed.digest,
        },
        live_image_fanout: "disconnected",
        legacy_messagepack_bytes: "verified_by_terminal_image_ipc",
    };
    write_evidence(evidence_path, &evidence)?;
    if policy.limits() != ImageLimits::V1 {
        return Err("process policy changed during the probe".to_owned());
    }
    Ok(())
}

async fn route_chunk(
    session: &mut PtyTerminalImageState,
    bytes: Vec<u8>,
    client: &Rc<RefCell<ObservedSink>>,
    term: &Rc<RefCell<ObservedSink>>,
) -> Result<SessionTerminalCommit, SessionTerminalError> {
    let client = Rc::clone(client);
    let term = Rc::clone(term);
    process_pty_reader_ingress(
        session,
        bytes,
        move |bytes| {
            client.borrow_mut().observe(bytes);
        },
        move |_observer, bytes, image_result| async move {
            term.borrow_mut().observe(bytes.as_ref());
            (image_result, None)
        },
        |_rejection| {},
    )
    .await
}

fn shared_session() -> Result<(PtyTerminalImageState, Arc<TerminalImageProcessPolicy>), String> {
    let policy = TerminalImageProcessPolicy::v1();
    let sibling_policy = TerminalImageProcessPolicy::v1();
    let session = PtyTerminalImageState::new(Arc::clone(&policy));
    let sibling = PtyTerminalImageState::new(Arc::clone(&sibling_policy));
    if !Arc::ptr_eq(&policy, &sibling_policy) || sibling.state().generation.0 != 1 {
        return Err("sessions do not share one process policy".to_owned());
    }
    if policy.limits() != ImageLimits::V1 {
        return Err("process policy does not preserve frozen v1 limits".to_owned());
    }
    Ok((session, policy))
}

async fn verify_pending_transfer(
    session: &mut PtyTerminalImageState,
    client: &Rc<RefCell<ObservedSink>>,
    term: &Rc<RefCell<ObservedSink>>,
) -> Result<(), String> {
    let first =
        route_chunk(session, b"before\x1b_Ga=q,f=24,s=1,v=1,i=1;/wAA".to_vec(), client, term)
            .await
            .map_err(|error| error.to_string())?;
    if first.outputs.len() != 1
        || !matches!(
            first.outputs.first(),
            Some(SessionTerminalOutput::Raw(raw)) if raw.bytes == b"before"
        )
    {
        return Err(format!("first read did not expose only prior raw bytes: {:?}", first.outputs));
    }
    let pending = session
        .state()
        .pending_transfer
        .ok_or_else(|| "split Kitty transfer has no pending metadata".to_owned())?;
    if pending.protocol != GraphicsProtocol::Kitty
        || pending.retained_payload_bytes != 4
        || pending.discarding
    {
        return Err(format!("split Kitty metadata drifted: {pending:?}"));
    }
    Ok(())
}

async fn verify_ordered_completion(
    session: &mut PtyTerminalImageState,
    client: &Rc<RefCell<ObservedSink>>,
    term: &Rc<RefCell<ObservedSink>>,
) -> Result<Vec<&'static str>, String> {
    let second = route_chunk(session, b"\x1b\\after\x1b[?80h".to_vec(), client, term)
        .await
        .map_err(|error| error.to_string())?;
    let order = second
        .outputs
        .iter()
        .map(|output| match output {
            SessionTerminalOutput::Image {
                sequence: TerminalOutputSequence(1),
                boundary: TerminalImageBoundary::Kitty(_),
                ..
            } => Ok("kitty"),
            SessionTerminalOutput::Raw(raw) if raw.bytes == b"after" => Ok("raw"),
            SessionTerminalOutput::Raw(raw) if raw.bytes == b"\x1b[?80h" => Ok("mode_raw"),
            SessionTerminalOutput::Image {
                sequence: TerminalOutputSequence(2),
                boundary:
                    TerminalImageBoundary::SixelMode { mode: SixelMode::Display, enabled: true },
                ..
            } => Ok("mode_image"),
            other => Err(format!("unexpected ordered output: {other:?}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if order != ["kitty", "raw", "mode_raw", "mode_image"] {
        return Err(format!("raw/image ordering drifted: {order:?}"));
    }
    Ok(order)
}

fn verify_owned_state(session: &mut PtyTerminalImageState) -> Result<StateOwnership, String> {
    let state = session.state();
    if state.pending_transfer.is_some()
        || state.generation.0 != 1
        || state.sequence.0 != 2
        || state.active_screen != TerminalScreenKind::Primary
        || state.definition_count != 0
        || state.placement_count != 0
    {
        return Err(format!("session-owned state drifted: {state:?}"));
    }
    Ok(StateOwnership {
        generation: state.generation.0,
        sequence: state.sequence.0,
        active_screen: "primary",
        definition_count: state.definition_count,
        placement_count: state.placement_count,
    })
}

async fn verify_direct_edge_cases() -> Result<(), String> {
    let mut session = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    let client = Rc::new(RefCell::new(ObservedSink::default()));
    let term = Rc::new(RefCell::new(ObservedSink::default()));

    let empty = route_chunk(&mut session, Vec::new(), &client, &term)
        .await
        .map_err(|error| error.to_string())?;
    if !empty.outputs.is_empty() || session.state().sequence.0 != 0 {
        return Err(format!("empty direct read mutated state: {empty:?}"));
    }

    let fallback = route_chunk(&mut session, b"\x1bX".to_vec(), &client, &term)
        .await
        .map_err(|error| error.to_string())?;
    verify_candidate_fallback(&fallback)?;

    let multiple = route_chunk(
        &mut session,
        b"\x1b_Ga=t,f=9;AAAA\x1b\\\x1b[?80h\x1b[?8452l".to_vec(),
        &client,
        &term,
    )
    .await
    .map_err(|error| error.to_string())?;
    verify_multiple_boundaries(&multiple)?;

    let work = session.framing_work();
    if work.direct_reads != 3 || work.speculative_clone_reads != 0 {
        return Err(format!("direct edge cases used speculative framing: {work:?}"));
    }
    if *client.borrow() != *term.borrow() || client.borrow().calls != 3 {
        return Err(format!(
            "edge-case routing drifted: client={:?} term={:?}",
            client.borrow(),
            term.borrow()
        ));
    }
    Ok(())
}

fn verify_candidate_fallback(fallback: &SessionTerminalCommit) -> Result<(), String> {
    let fallback_bytes = fallback
        .outputs
        .iter()
        .flat_map(|output| match output {
            SessionTerminalOutput::Raw(raw) => raw.bytes.as_slice(),
            SessionTerminalOutput::Image { .. } => &[],
        })
        .copied()
        .collect::<Vec<_>>();
    if fallback_bytes != b"\x1bX"
        || !matches!(
            fallback.outputs.as_slice(),
            [SessionTerminalOutput::Raw(first), SessionTerminalOutput::Raw(second)]
                if first.range.start == 0
                    && first.range.end == second.range.start
                    && second.range.end == 2
        )
    {
        return Err(format!("candidate fallback did not return exact raw bytes: {fallback:?}"));
    }
    Ok(())
}

fn verify_multiple_boundaries(multiple: &SessionTerminalCommit) -> Result<(), String> {
    let image_boundaries = multiple
        .outputs
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Image { sequence, boundary, .. } => Some((sequence.0, boundary)),
            SessionTerminalOutput::Raw(_) => None,
        })
        .collect::<Vec<_>>();
    let ordered = match image_boundaries.as_slice() {
        [
            (1, TerminalImageBoundary::Failure(failure)),
            (2, TerminalImageBoundary::SixelMode { mode: SixelMode::Display, enabled: true }),
            (3, TerminalImageBoundary::SixelMode { mode: SixelMode::CursorRight, enabled: false }),
        ] => failure.category == GraphicsFailureCategory::UnsupportedAction,
        _ => false,
    };
    if !ordered {
        return Err(format!("failure or multiple-boundary ordering drifted: {multiple:?}"));
    }
    Ok(())
}

async fn verify_large_split_transfer() -> Result<FramingWorkEvidence, String> {
    const TRANSFER_BYTES: usize = 8 * 1024 * 1024;
    const SPLIT_READ_BYTES: usize = 64 * 1024;

    let mut session = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    let client = Rc::new(RefCell::new(ObservedSink::default()));
    let term = Rc::new(RefCell::new(ObservedSink::default()));
    let prefix = route_chunk(&mut session, b"\x1bPq".to_vec(), &client, &term)
        .await
        .map_err(|error| error.to_string())?;
    if !prefix.outputs.is_empty() {
        return Err(format!("large Sixel prefix emitted early: {prefix:?}"));
    }

    for _ in 0..(TRANSFER_BYTES / SPLIT_READ_BYTES) {
        let chunk = vec![b'?'; SPLIT_READ_BYTES];
        let commit = route_chunk(&mut session, chunk, &client, &term)
            .await
            .map_err(|error| error.to_string())?;
        if !commit.outputs.is_empty() {
            return Err(format!("large split Sixel emitted before terminator: {commit:?}"));
        }
    }
    let pending = session
        .state()
        .pending_transfer
        .ok_or_else(|| "large split Sixel lost pending transfer".to_owned())?;
    if pending.protocol != GraphicsProtocol::Sixel
        || pending.retained_payload_bytes != TRANSFER_BYTES
        || pending.discarding
    {
        return Err(format!("large split Sixel metadata drifted: {pending:?}"));
    }

    let completed = route_chunk(&mut session, b"\x1b\\".to_vec(), &client, &term)
        .await
        .map_err(|error| error.to_string())?;
    if !matches!(
        completed.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            sequence: TerminalOutputSequence(1),
            boundary: TerminalImageBoundary::Sixel(_),
            ..
        }]
    ) || session.state().pending_transfer.is_some()
    {
        return Err(format!("large split Sixel did not commit once: {completed:?}"));
    }

    let work = session.framing_work();
    let client = *client.borrow();
    let term = *term.borrow();
    let expected_reads = u64::try_from(TRANSFER_BYTES / SPLIT_READ_BYTES)
        .ok()
        .and_then(|reads| reads.checked_add(2))
        .ok_or_else(|| "large split read count overflowed".to_owned())?;
    if work.direct_reads != expected_reads
        || work.speculative_clone_reads != 0
        || client != term
        || client.calls != expected_reads
    {
        return Err(format!(
            "large split work/routing drifted: work={work:?} client={client:?} term={term:?}"
        ));
    }

    Ok(FramingWorkEvidence {
        large_transfer_bytes: TRANSFER_BYTES,
        split_read_bytes: SPLIT_READ_BYTES,
        direct_reads: work.direct_reads,
        speculative_clone_reads: work.speculative_clone_reads,
        direct_edge_cases: "pending_completion_mode_raw_image_failure_fallback_empty_multiple",
        client_delivery_calls: client.calls,
        term_feed_calls: term.calls,
        matching_digest: client.digest == term.digest,
    })
}

async fn verify_transactional_exhaustion() -> Result<TransactionalExhaustionEvidence, String> {
    let exhausted_policy = TerminalImageProcessPolicy::with_sequence_ceiling_for_validation(0);
    let mut pending = PtyTerminalImageState::new(Arc::clone(&exhausted_policy));
    let client = Rc::new(RefCell::new(ObservedSink::default()));
    let term = Rc::new(RefCell::new(ObservedSink::default()));
    let partial =
        route_chunk(&mut pending, b"\x1b_Ga=q,f=24,s=1,v=1,i=1;/wAA".to_vec(), &client, &term)
            .await
            .map_err(|error| error.to_string())?;
    if !partial.outputs.is_empty() {
        return Err(format!("partial transfer unexpectedly emitted output: {:?}", partial.outputs));
    }
    let before_rejection = pending.state();
    if !matches!(
        route_chunk(&mut pending, b"\x1b\\".to_vec(), &client, &term).await,
        Err(SessionTerminalError::SequenceExhausted)
    ) {
        return Err("sequence ceiling did not produce typed exhaustion".to_owned());
    }
    let after_rejection = pending.state();
    if after_rejection != before_rejection {
        return Err(format!(
            "exhaustion mutated pending or canonical state: {before_rejection:?} -> {after_rejection:?}"
        ));
    }
    let pending_work = pending.framing_work();
    if pending_work.speculative_clone_reads != 2 {
        return Err(format!("near-exhaustion rollback work drifted: {pending_work:?}"));
    }

    let mut offset = PtyTerminalImageState::new(exhausted_policy);
    let offset_client = Rc::new(RefCell::new(ObservedSink::default()));
    let offset_term = Rc::new(RefCell::new(ObservedSink::default()));
    if !matches!(
        route_chunk(&mut offset, b"\x1b[?80h".to_vec(), &offset_client, &offset_term).await,
        Err(SessionTerminalError::SequenceExhausted)
    ) {
        return Err("mode boundary did not exhaust the zero sequence ceiling".to_owned());
    }
    let raw = route_chunk(&mut offset, b"plain".to_vec(), &offset_client, &offset_term)
        .await
        .map_err(|error| error.to_string())?;
    if raw.outputs.len() != 1
        || !matches!(
            raw.outputs.first(),
            Some(SessionTerminalOutput::Raw(raw))
                if raw.bytes == b"plain" && raw.range.start == 0 && raw.range.end == 5
        )
    {
        return Err(format!("rejected input advanced the production framer: {:?}", raw.outputs));
    }

    Ok(TransactionalExhaustionEvidence {
        typed_rejection: "pass",
        state_unchanged: "pass",
        offset_unconsumed: "pass",
        speculative_clone_reads: pending_work.speculative_clone_reads,
    })
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence<'_>) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode seam evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
