//! Production-path evidence for deterministic terminal-image storage accounting.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Barrier;

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{ImageLimits, TerminalScreenKind};
use scribe_image_decode::{
    DecodeStorage, StorageProcess, StorageValidation, StorageValidationRejection,
};
use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
use scribe_pty::graphics_framing::{
    GraphicsEvent, GraphicsFailureCategory, GraphicsFramer, GraphicsStorageRejection,
};
use scribe_server::session_manager::build_term_config;
use scribe_server::terminal_image_state::{
    ImageStorageClassCounters, ImageStorageCounters, ImageStorageDigests, ImageStorageOwnership,
    ProductionTerminalFeed, PtyReaderIngressRejection, PtyTerminalImageState,
    SessionTerminalCommit, SessionTerminalError, SessionTerminalOutput, SessionTerminalState,
    StorageAllocationClass, StorageLedgerOperation, StorageLedgerScope,
    StorageLedgerValidationFault, StorageSnapshotValidationFault, TerminalImageBoundary,
    TerminalImageProcessPolicy, feed_terminal_image_result_production, process_pty_reader_ingress,
};
use serde::Serialize;
use tokio::sync::mpsc;
use vte::ansi::Processor;

/// One-pixel PNG shared by production-format and work-admission evidence.
const PNG_FIXTURE: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg==";

#[derive(Serialize)]
struct Evidence {
    schema_version: u32,
    status: &'static str,
    engine: &'static str,
    metric: &'static str,
    exact_limit: BoundaryEvidence,
    replacement: ReplacementEvidence,
    kitty: ProtocolEvidence,
    sixel: ProtocolEvidence,
    ledger_atomicity: LedgerAtomicityEvidence,
    observed_capacity: ObservedCapacityEvidence,
    cross_session: CrossSessionEvidence,
    concurrent_release: ConcurrentReleaseEvidence,
    ingress_faults: IngressFaultEvidence,
    multi_event_rollback: MultiEventRollbackEvidence,
    formats: Vec<FormatEvidence>,
    kitty_chunks: KittyChunkEvidence,
    metadata: MetadataEvidence,
    framer_retry: FramerRetryEvidence,
    event_ownership: EventOwnershipEvidence,
    grid_observations: GridObservationEvidence,
    work_admission: WorkAdmissionEvidence,
    final_process_current: u64,
}

/// Proof that consuming a metadata vector keeps its ledger ownership until the
/// backing allocation is actually freed.
#[derive(Serialize)]
struct EventOwnershipEvidence {
    outputs_requested: u64,
    charged_while_iterating: EvidenceCheck,
    charged_after_partial_drain: EvidenceCheck,
    released_after_iterator_drop: EvidenceCheck,
}

/// Proof that grid observations and effect vectors are reserved from the same
/// paired ledger before they are allocated.
#[derive(Serialize)]
struct GridObservationEvidence {
    effect_count: usize,
    class_current_while_held: u64,
    class_peak_while_held: u64,
    accounted_before_allocation: EvidenceCheck,
    released_after_commit_drop: EvidenceCheck,
    rejection: &'static str,
    rejected_ledger_zero: EvidenceCheck,
}

/// Proof that work-budget admission gates decoder initialization instead of
/// trailing it: a refused decode reserves no decoded storage at all.
#[derive(Serialize)]
struct WorkAdmissionEvidence {
    admitted_work_units: u64,
    refused_initialization_work: u64,
    sixel_rejection: &'static str,
    sixel_decoded_peak: u64,
    no_storage_before_admission: EvidenceCheck,
    released_after_rejection: EvidenceCheck,
}

#[derive(Serialize)]
#[serde(transparent)]
struct EvidenceCheck(bool);

type StorageClassPair = (ImageStorageClassCounters, ImageStorageClassCounters);
type NamedStorageClassPair = (&'static str, StorageClassPair);
type AllocationStorageClassPair = (StorageAllocationClass, StorageClassPair);

impl From<bool> for EvidenceCheck {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

#[derive(Serialize)]
struct FramerRetryEvidence {
    candidate_exact_rollback: EvidenceCheck,
    candidate_retry_events: usize,
    active_exact_rollback: EvidenceCheck,
    active_retry_events: usize,
    eof_exact_rollback: EvidenceCheck,
    eof_retry_events: usize,
    no_duplicate_publication: EvidenceCheck,
}

#[derive(Serialize)]
struct MetadataEvidence {
    input_bytes: usize,
    event_requested_peak: u64,
    event_observed_peak: u64,
    output_requested_peak: u64,
    output_observed_peak: u64,
    measured_total_peak: u64,
    exact_success: bool,
    max_minus_one_rejection: &'static str,
    rollback_unchanged: bool,
    current_after_release: u64,
}

#[derive(Serialize)]
struct KittyChunkEvidence {
    aggregate_encoded_bytes: usize,
    aggregate_split_success: EvidenceCheck,
    individual_chunk_rejection: &'static str,
    chunk_count_rejection: &'static str,
    first_action_preserved: EvidenceCheck,
    first_ids_preserved: EvidenceCheck,
    first_quiet_preserved: EvidenceCheck,
    final_controls_preserved: EvidenceCheck,
    final_presence_preserved: EvidenceCheck,
    query_canonical_retained: usize,
    transmit_display_success: EvidenceCheck,
    pending_after_final: usize,
    equal_repeats_accepted: EvidenceCheck,
    conflicting_controls_rejected: EvidenceCheck,
    query_boundary_ordered: EvidenceCheck,
    query_publication_count: usize,
    current_after_release: u64,
}

#[derive(Serialize)]
struct FormatEvidence {
    id: &'static str,
    measured_peak: u64,
    retained_current: u64,
    decoded_requested: usize,
    decoded_observed: usize,
    decoded_digest: u64,
    exact_success: bool,
    max_minus_one_rejection: &'static str,
    rollback_unchanged: bool,
    current_after_release: u64,
}

#[derive(Serialize)]
struct BoundaryEvidence {
    requested: u64,
    observed: u64,
    kitty_exact: &'static str,
    kitty_max_plus_one: &'static str,
    sixel_exact: &'static str,
    sixel_max_plus_one: &'static str,
    rejection: &'static str,
    rejection_unchanged: bool,
    reservation_attempts: u64,
    allocator_attempts: u64,
    reserve_before_allocation_calls: u64,
}

#[derive(Serialize)]
struct ReplacementEvidence {
    old_requested: u64,
    new_requested: u64,
    requested_peak: u64,
    observed_peak: u64,
    failed_growth_rollback: bool,
    failed_replacement_rollback: bool,
    current_after_release: u64,
    required_peak: u64,
    enforced_limit: u64,
    reservation_attempt_delta: u64,
    allocator_attempt_delta: u64,
    reserve_before_allocation_delta: u64,
    reconciliation_delta: u64,
    framing_event_metadata_peak: u64,
    terminal_output_metadata_peak: u64,
    decoded_kitty_peak: u64,
}

#[derive(Serialize)]
struct ProtocolEvidence {
    retained_requested: usize,
    retained_observed: usize,
    completed_requested: usize,
    completed_observed: usize,
    replacement_peak: u64,
    typed_rejection: &'static str,
    rollback: bool,
    current_after_release: u64,
    storage_error: &'static str,
    routing: RoutingEvidence,
    reservation_attempt_delta: u64,
    allocator_attempt_delta: u64,
    reserve_call_delta: u64,
    event_release_exact: bool,
    rejection_before: CurrentPeakEvidence,
    rejection_after: CurrentPeakEvidence,
    attempt_histogram: Option<KittyAttemptHistogram>,
    sixel_storage: Option<SixelStorageEvidence>,
}

#[derive(Serialize)]
struct SixelStorageEvidence {
    dimensions: [usize; 2],
    classes: Vec<StorageClassCapacityEvidence>,
    decoded_capacities: [u64; 3],
    decoded_growth_overlap: u64,
    decoded_compaction_overlap: u64,
    body_digest: u64,
    decoded_digest: u64,
    exact_limit: u64,
    session_telemetry: StorageTelemetryEvidence,
    process_telemetry: StorageTelemetryEvidence,
    global_max_minus_stage: String,
    global_max_minus_overlap: [u64; 4],
}

#[derive(Serialize)]
struct StorageClassCapacityEvidence {
    class: &'static str,
    requested_current: u64,
    requested_peak: u64,
    observed_current: u64,
    observed_peak: u64,
}

#[derive(Serialize)]
struct StorageTelemetryEvidence {
    requested_current: u64,
    requested_peak: u64,
    observed_current: u64,
    observed_peak: u64,
    reservation_attempts: u64,
    allocator_attempts: u64,
    reserve_before_allocation_calls: u64,
    observed_reconciliations: u64,
}

#[derive(Serialize)]
struct KittyAttemptHistogram {
    stages: Vec<StorageClassStageEvidence>,
    reserve_only_checks: [&'static str; 3],
    targeted_failure_class: &'static str,
    targeted_failure_occurrence: u64,
    matching_reservations: u64,
    fired_rejections: u64,
    staged_allocations: u64,
    global_max_minus_scope: &'static str,
    final_rollback_scope: &'static str,
    session: AttemptDeltas,
    process: AttemptDeltas,
}

#[derive(Serialize)]
struct StorageClassStageEvidence {
    class: &'static str,
    reservations: u64,
    allocator_attempts: u64,
    reserve_before_allocation_calls: u64,
    reconciliations: u64,
    reserve_only_checks: u64,
}

#[derive(Serialize)]
struct CurrentPeakEvidence {
    session_current: u64,
    session_peak: u64,
    process_current: u64,
    process_peak: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct AttemptDeltas {
    reservation: u64,
    allocator: u64,
    reserve_call: u64,
    reconcile: u64,
}

#[derive(Serialize)]
struct LedgerAtomicityEvidence {
    requested_counter_overflow: &'static str,
    observed_counter_overflow: &'static str,
    reservation_counter_overflow: &'static str,
    allocation_counter_overflow: &'static str,
    reconciliation_counter_overflow: &'static str,
    poisoned_ledger: &'static str,
    paired_partial_charge_prevented: bool,
    reserve_mixed_precedence: &'static str,
    reconcile_mixed_precedence: &'static str,
    mixed_rejections_unchanged: bool,
}

#[derive(Serialize)]
struct ObservedCapacityEvidence {
    framer_requested: usize,
    framer_observed: usize,
    requested: u64,
    observed: u64,
    reconciliations: u64,
    failed_reconcile_typed: &'static str,
    failed_reconcile_rollback: bool,
    failed_reconcile_allocator_attempts_before: u64,
    failed_reconcile_allocator_attempts: u64,
    failed_reconcile_target_class: &'static str,
    failed_reconcile_target_occurrence: u64,
    failed_reconcile_reservations: u64,
    failed_reconcile_reserve_before: u64,
    failed_reconcile_reconciliations: u64,
}

#[derive(Serialize)]
struct CrossSessionEvidence {
    process_current_at_limit: u64,
    process_peak: u64,
    typed_rejection: &'static str,
    foreign_session_unchanged: bool,
    current_after_release: u64,
    required_peak: u64,
    enforced_limit: u64,
    reservation_attempts: u64,
    allocator_attempts: u64,
    reserve_before_allocation_calls: u64,
    setup_reservation_attempts: u64,
    setup_allocator_attempts: u64,
    setup_reserve_before_allocation_calls: u64,
    success_reservation_attempts: u64,
    success_allocator_attempts: u64,
    success_reserve_before_allocation_calls: u64,
    rejection_reservation_delta: u64,
    rejection_allocator_delta: u64,
    rejection_reserve_before_delta: u64,
}

#[derive(Serialize)]
struct ConcurrentReleaseEvidence {
    detached_requested: usize,
    detached_outputs_requested: usize,
    detached_total_requested: usize,
    in_flight_process_peak: u64,
    process_current_before: u64,
    process_current_after_external_release: u64,
    process_current_after_failure: u64,
    process_peak_after_failure: u64,
    session_current_after_failure: u64,
    provisional_release_exact: &'static str,
    external_release_applied: &'static str,
    failed_peak_unpublished: &'static str,
    class_states_exact: &'static str,
    invariant_error: &'static str,
    no_deadlock: &'static str,
    final_process_current: u64,
}

struct ConcurrentReleaseCheck<'a> {
    result: &'a Result<SessionTerminalCommit, SessionTerminalError>,
    detached_requested: usize,
    detached_outputs_requested: usize,
    detached_total_requested: usize,
    before: ImageStorageCounters,
    after_external_release: ImageStorageCounters,
    pressured_session: ImageStorageCounters,
    process_after_failure: ImageStorageCounters,
    external_release_applied: &'static str,
    provisional_release_exact: &'static str,
    failed_peak_unpublished: &'static str,
    foreign_unchanged: &'static str,
    class_states_exact: &'static str,
    failed_sequence: u64,
}

#[derive(Serialize)]
struct IngressFaultEvidence {
    candidate_error: &'static str,
    active_error: &'static str,
    canonical_error: &'static str,
    routing: RoutingEvidence,
}

#[derive(Serialize)]
struct MultiEventRollbackEvidence {
    error: &'static str,
    prior_sequence: u64,
    sequence_after: u64,
    prior_sixel_digest: u64,
    sixel_digest_after: u64,
    state_before: CanonicalStateEvidence,
    state_after: CanonicalStateEvidence,
    accounting_before: CurrentPeakEvidence,
    accounting_after: CurrentPeakEvidence,
    state_rollback: StateRollbackEvidence,
    storage_rollback: StorageRollbackEvidence,
    canonical_unchanged: bool,
    allocation_class: &'static str,
    matching_allocation_attempts: u64,
    staged_before_failure: u64,
    targeted_rejection_fired: u64,
    routing: RoutingEvidence,
}

#[derive(Serialize)]
struct CanonicalStateEvidence {
    generation: u64,
    sequence: u64,
    active_screen: &'static str,
    definition_count: usize,
    placement_count: usize,
    pending_transfer: bool,
}

#[derive(Serialize)]
struct StateRollbackEvidence {
    state_unchanged: bool,
    ownership_unchanged: bool,
}

#[derive(Serialize)]
struct StorageRollbackEvidence {
    digests_unchanged: bool,
    staged_release_exact: bool,
}

#[derive(Serialize)]
struct RoutingEvidence {
    delivery: DeliveryEvidence,
    matching_digest: bool,
    rejection: RejectionRoutingEvidence,
}

#[derive(Serialize)]
struct RejectionRoutingEvidence {
    rejection_callback_once: bool,
    rejection_payload_free: bool,
}

#[derive(Serialize)]
struct DeliveryEvidence {
    client_delivery_once: bool,
    term_feed_once: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservedSink {
    calls: u64,
    bytes: u64,
    digest: u64,
    rejection_calls: u64,
    rejection: Option<PtyReaderIngressRejection>,
    real_term_feed: bool,
    term_cursor_column: usize,
    term_cell: char,
    observer_cursor_column: u16,
}

struct AccountingDimensions;

impl Dimensions for AccountingDimensions {
    fn total_lines(&self) -> usize {
        24
    }

    fn screen_lines(&self) -> usize {
        24
    }

    fn columns(&self) -> usize {
        80
    }
}

struct RealTermFeed {
    term: Arc<tokio::sync::Mutex<Term<ScribeEventListener>>>,
    processor: Processor,
    observed: ObservedSink,
    _events: mpsc::UnboundedReceiver<SessionEvent>,
}

impl RealTermFeed {
    fn new() -> Self {
        let (event_tx, events) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(SessionId::new(), event_tx);
        Self {
            term: Arc::new(tokio::sync::Mutex::new(Term::new(
                build_term_config(32),
                &AccountingDimensions,
                listener,
            ))),
            processor: Processor::new(),
            observed: ObservedSink::default(),
            _events: events,
        }
    }
}

struct FaultObservation {
    client: ObservedSink,
    term: ObservedSink,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
struct MixedSnapshotCase {
    session_limit: u64,
    process_limit: u64,
    observed_capacity_extra: usize,
    fault: StorageSnapshotValidationFault,
}

impl ObservedSink {
    fn observe(&mut self, bytes: &[u8]) {
        self.calls = self.calls.saturating_add(1);
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.bytes = self.bytes.saturating_add(byte_len);
        self.digest = bytes.iter().fold(self.digest, |digest, byte| {
            digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
        });
    }

    fn observe_rejection(&mut self, rejection: PtyReaderIngressRejection) {
        self.rejection_calls = self.rejection_calls.saturating_add(1);
        self.rejection = Some(rejection);
    }
}

// @lat: [[test#Test Harness#Terminal Image Storage Accounting#Production Accounting Probe]]
pub fn run(evidence_path: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create accounting runtime: {error}"))?;
    runtime.block_on(run_probe(evidence_path))
}

async fn run_probe(evidence_path: &Path) -> Result<(), String> {
    let exact_limit =
        verify_exact_boundary().map_err(|error| format!("exact boundary: {error}"))?;
    let replacement =
        verify_replacement_peaks().map_err(|error| format!("replacement: {error}"))?;
    let kitty =
        Box::pin(verify_kitty_paths()).await.map_err(|error| format!("Kitty paths: {error}"))?;
    let sixel = verify_sixel_paths().await.map_err(|error| format!("Sixel paths: {error}"))?;
    let ledger_atomicity =
        verify_ledger_atomicity().map_err(|error| format!("ledger atomicity: {error}"))?;
    let observed_capacity =
        verify_observed_capacity().await.map_err(|error| format!("observed capacity: {error}"))?;
    let (cross_session, final_process_current) =
        verify_cross_session().map_err(|error| format!("cross session: {error}"))?;
    let concurrent_release =
        verify_concurrent_release().map_err(|error| format!("concurrent release: {error}"))?;
    let ingress_faults =
        verify_ingress_faults().await.map_err(|error| format!("ingress faults: {error}"))?;
    let multi_event_rollback = verify_multi_event_rollback()
        .await
        .map_err(|error| format!("multi-event rollback: {error}"))?;
    let formats = verify_production_formats().map_err(|error| format!("formats: {error}"))?;
    let kitty_chunks =
        verify_kitty_chunk_protocol().map_err(|error| format!("Kitty chunks: {error}"))?;
    let metadata = verify_metadata_boundaries().map_err(|error| format!("metadata: {error}"))?;
    let framer_retry =
        verify_framer_retry_faults().map_err(|error| format!("framer retry: {error}"))?;
    let event_ownership =
        verify_event_ownership().map_err(|error| format!("event ownership: {error}"))?;
    let grid_observations = verify_grid_observation_accounting()
        .await
        .map_err(|error| format!("grid observations: {error}"))?;
    let work_admission =
        verify_work_admission().map_err(|error| format!("work admission: {error}"))?;
    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "scribe-server SessionTerminal production storage owner",
        metric: "requested live storage with allocator-observed retained capacity",
        exact_limit,
        replacement,
        kitty,
        sixel,
        ledger_atomicity,
        observed_capacity,
        cross_session,
        concurrent_release,
        ingress_faults,
        multi_event_rollback,
        formats,
        kitty_chunks,
        metadata,
        framer_retry,
        event_ownership,
        grid_observations,
        work_admission,
        final_process_current,
    };
    write_evidence(evidence_path, &evidence)
}

fn verify_exact_boundary() -> Result<BoundaryEvidence, String> {
    let kitty_bytes = kitty_rgba(1, 1, &[255, 0, 0, 128]);
    let mut baseline = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let baseline_commit = baseline
        .process_bytes(&kitty_bytes)
        .map_err(|error| format!("baseline Kitty decode failed: {error}"))?;
    let exact_peak = counters(&baseline)?.0.requested_peak;
    drop(baseline_commit);
    baseline.release_retained_storage();

    let policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(
        exact_peak,
        exact_peak.saturating_mul(2),
    );
    let mut session = PtyTerminalImageState::new(policy);
    let exact_commit = session
        .process_bytes(&kitty_bytes)
        .map_err(|error| format!("exact Kitty production decode failed: {error}"))?;
    let (exact, _) = counters(&session)?;
    drop(exact_commit);
    session.release_retained_storage();
    let released_pair = counters(&session)?;
    let released = released_pair.0;
    if released.requested_current != 0 || released.observed_current != 0 {
        return Err(format!("exact allocation did not release to zero: {released:?}"));
    }
    let mut rejected_session =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            exact_peak.saturating_sub(1),
            exact_peak.saturating_mul(2),
        ));
    let rejection_before = counters(&rejected_session)?;
    let rejection = rejected_session.process_bytes(&kitty_bytes);
    if !matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) {
        return Err(format!("max-plus-one was not a typed session rejection: {rejection:?}"));
    }
    let after_rejection_pair = counters(&rejected_session)?;
    let after_rejection = after_rejection_pair.0;
    let rejection_unchanged = ownership_counters_equal(rejection_before, after_rejection_pair);
    if !rejection_unchanged {
        return Err(format!("max-plus-one changed live counters: {after_rejection:?}"));
    }
    verify_exact_sixel_boundary()?;
    Ok(BoundaryEvidence {
        requested: exact_peak,
        observed: exact.observed_peak,
        kitty_exact: "pass",
        kitty_max_plus_one: "pass",
        sixel_exact: "pass",
        sixel_max_plus_one: "pass",
        rejection: "session_limit",
        rejection_unchanged,
        reservation_attempts: after_rejection.reservation_attempts,
        allocator_attempts: after_rejection.allocator_attempts,
        reserve_before_allocation_calls: after_rejection.reserve_before_allocation_calls,
    })
}

fn verify_exact_sixel_boundary() -> Result<(), String> {
    let sixel_bytes = b"\x1bPq~\x1b\\";
    let mut sixel_baseline = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let sixel_commit = sixel_baseline
        .process_bytes(sixel_bytes)
        .map_err(|error| format!("baseline Sixel decode failed: {error}"))?;
    let sixel_peak = counters(&sixel_baseline)?.0.requested_peak;
    drop(sixel_commit);
    sixel_baseline.release_retained_storage();
    let mut sixel_exact =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            sixel_peak,
            sixel_peak.saturating_mul(2),
        ));
    let sixel_exact_commit = sixel_exact
        .process_bytes(sixel_bytes)
        .map_err(|error| format!("exact Sixel production decode failed: {error}"))?;
    drop(sixel_exact_commit);
    sixel_exact.release_retained_storage();
    let mut sixel =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            sixel_peak.saturating_sub(1),
            sixel_peak.saturating_mul(2),
        ));
    let sixel_rejection = sixel.process_bytes(sixel_bytes);
    if !matches!(
        sixel_rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) {
        return Err(format!(
            "Sixel max-plus-one was not a typed session rejection: {sixel_rejection:?}"
        ));
    }
    Ok(())
}

fn verify_replacement_peaks() -> Result<ReplacementEvidence, String> {
    let first_bytes = kitty_rgba(1, 1, &[255, 0, 0, 255]);
    let second_bytes = kitty_rgba(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
    let policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX);
    let mut session = PtyTerminalImageState::new(policy);
    drop(session.process_bytes(&first_bytes).map_err(|error| error.to_string())?);
    let before_replacement = counters(&session)?.0;
    let old_requested = before_replacement.requested_current;
    drop(session.process_bytes(&second_bytes).map_err(|error| error.to_string())?);
    let (success, _) = counters(&session)?;
    let framing_events = session
        .validation_storage_class_counters(StorageAllocationClass::FramingEvents)
        .map_err(|error| error.to_string())?
        .0;
    let terminal_outputs = session
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?
        .0;
    let decoded_kitty = session
        .validation_storage_class_counters(StorageAllocationClass::DecodedKitty)
        .map_err(|error| error.to_string())?
        .0;
    if success.requested_peak <= success.requested_current
        || success.requested_peak <= old_requested
    {
        return Err(format!("replacement peak did not hold old plus new: {success:?}"));
    }
    let required_peak = success.requested_peak;
    let new_requested = success.requested_current;
    session.release_retained_storage();
    let (released, _) = counters(&session)?;

    let (growth_rollback, replacement_rollback, failed_current, failure_deltas) =
        verify_replacement_rejections(&first_bytes, &second_bytes, required_peak)?;
    Ok(ReplacementEvidence {
        old_requested,
        new_requested,
        requested_peak: success.requested_peak,
        observed_peak: success.observed_peak,
        failed_growth_rollback: growth_rollback,
        failed_replacement_rollback: replacement_rollback,
        current_after_release: released.requested_current + failed_current,
        required_peak,
        enforced_limit: required_peak.saturating_sub(1),
        reservation_attempt_delta: failure_deltas.reservation,
        allocator_attempt_delta: failure_deltas.allocator,
        reserve_before_allocation_delta: failure_deltas.reserve_call,
        reconciliation_delta: failure_deltas.reconcile,
        framing_event_metadata_peak: framing_events.requested_peak,
        terminal_output_metadata_peak: terminal_outputs.requested_peak,
        decoded_kitty_peak: decoded_kitty.requested_peak,
    })
}

fn verify_replacement_rejections(
    first_bytes: &[u8],
    second_bytes: &[u8],
    required_peak: u64,
) -> Result<(bool, bool, u64, AttemptDeltas), String> {
    let failure_policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(
        required_peak.saturating_sub(1),
        u64::MAX,
    );
    let mut failed = PtyTerminalImageState::new(failure_policy);
    drop(failed.process_bytes(first_bytes).map_err(|error| error.to_string())?);
    let before = counters(&failed)?;
    let before_owner = failed.storage_ownership();
    let before_state = failed.state();
    let before_digests = failed.validation_storage_digests();
    let growth = failed.process_bytes(second_bytes);
    let after_growth = counters(&failed)?;
    let after_growth_owner = failed.storage_ownership();
    let replacement = failed.process_bytes(second_bytes);
    let after_replacement = counters(&failed)?;
    let after_replacement_owner = failed.storage_ownership();
    let typed = |result: &Result<SessionTerminalCommit, SessionTerminalError>| {
        matches!(result, Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit)))
    };
    let growth_deltas = attempt_deltas("Kitty replacement", before.0, after_growth.0)?;
    let exact_attempts = |after: (ImageStorageCounters, ImageStorageCounters), multiplier: u64| {
        [after.0, after.1].into_iter().all(|scope| {
            scope.reservation_attempts
                == before
                    .0
                    .reservation_attempts
                    .saturating_add(growth_deltas.reservation.saturating_mul(multiplier))
                && scope.allocator_attempts
                    == before
                        .0
                        .allocator_attempts
                        .saturating_add(growth_deltas.allocator.saturating_mul(multiplier))
                && scope.reserve_before_allocation_calls
                    == before
                        .0
                        .reserve_before_allocation_calls
                        .saturating_add(growth_deltas.reserve_call.saturating_mul(multiplier))
                && scope.observed_reconciliations
                    == before
                        .0
                        .observed_reconciliations
                        .saturating_add(growth_deltas.reconcile.saturating_mul(multiplier))
        })
    };
    let growth_rollback = typed(&growth)
        && ownership_counters_equal(before, after_growth)
        && exact_attempts(after_growth, 1)
        && before_owner == after_growth_owner
        && before_state == failed.state()
        && before_digests == failed.validation_storage_digests();
    let replacement_rollback = typed(&replacement)
        && ownership_counters_equal(before, after_replacement)
        && exact_attempts(after_replacement, 2)
        && before_owner == after_replacement_owner
        && before_state == failed.state()
        && before_digests == failed.validation_storage_digests();
    if !growth_rollback || !replacement_rollback {
        return Err(format!(
            "failed growth/replacement changed canonical storage: growth={growth:?} \
             replacement={replacement:?} before={before:?} after_growth={after_growth:?} \
             after_replacement={after_replacement:?} before_owner={before_owner:?} \
             after_growth_owner={after_growth_owner:?} \
             after_replacement_owner={after_replacement_owner:?} \
             target_class=production_kitty_final"
        ));
    }
    failed.release_retained_storage();
    let failed_released = counters(&failed)?.0;
    Ok((growth_rollback, replacement_rollback, failed_released.requested_current, growth_deltas))
}

async fn verify_kitty_paths() -> Result<ProtocolEvidence, String> {
    let first_bytes = b"\x1b_Gf=32,s=1,v=1,m=1;AAAA\x1b\\".to_vec();
    let rejected_bytes = b"A\x1b_Gm=1;BBBB\x1b\\".to_vec();
    let required_peak = measure_kitty_replacement_peak(&first_bytes, &rejected_bytes).await?;
    let mut session = limited_kitty_session(first_bytes).await?;
    let before = session.storage_ownership();
    let before_digests = session.validation_storage_digests();
    let before_state = session.state();
    let before_pair = counters(&session)?;
    let before_counters = before_pair.0;
    let (rejected, client, term) = route_observed(&mut session, rejected_bytes.clone()).await;
    let typed = matches!(
        &rejected,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    );
    let (delivery, matching_digest) = delivery_checks(&rejected_bytes, client, term);
    let DeliveryEvidence { client_delivery_once, term_feed_once } = delivery;
    let after = session.storage_ownership();
    let after_digests = session.validation_storage_digests();
    let after_state = session.state();
    let after_pair = counters(&session)?;
    let after_counters = after_pair.0;
    let deltas = attempt_deltas("Kitty", before_counters, after_counters)?;
    let process_deltas = attempt_deltas("Kitty process", before_pair.1, after_pair.1)?;
    let rejection_observation = session.validation_rejection_observation();
    let attempt_histogram = kitty_attempt_histogram(deltas, process_deltas, rejection_observation)?;
    let event_release_exact = ownership_counters_equal(before_pair, after_pair);
    let rollback = before == after
        && before_digests == after_digests
        && before_state == after_state
        && before.pending_kitty_requested == 3
        && event_release_exact
        && deltas
            == AttemptDeltas { reservation: 10, allocator: 7, reserve_call: 10, reconcile: 7 }
        && process_deltas == deltas;
    let (expected_client, expected_term) = expected_kitty_sinks(&rejected_bytes, before_state);
    if !typed
        || !rollback
        || !client_delivery_once
        || !term_feed_once
        || !matching_digest
        || client != expected_client
        || term != expected_term
    {
        return Err(format!(
            "Kitty replacement rejection drifted: result={rejected:?} owner={before:?}->{after:?} \
             digests={before_digests:?}->{after_digests:?} state={before_state:?}->{after_state:?} \
             counters={before_pair:?}->{after_pair:?} client={client:?}/{expected_client:?} \
             term={term:?}/{expected_term:?} deltas={:?}",
            (deltas.reservation, deltas.allocator, deltas.reserve_call, deltas.reconcile),
        ));
    }
    session.release_retained_storage();
    let (released, _) = counters(&session)?;
    let (completed_requested, completed_observed) = verify_completed_kitty().await?;
    Ok(ProtocolEvidence {
        retained_requested: before.pending_kitty_requested,
        retained_observed: before.pending_kitty_observed,
        completed_requested,
        completed_observed,
        replacement_peak: required_peak,
        typed_rejection: "session_limit",
        rollback,
        current_after_release: released.requested_current,
        storage_error: "session_limit",
        routing: routing_evidence(
            DeliveryEvidence { client_delivery_once, term_feed_once },
            matching_digest,
            client,
        ),
        reservation_attempt_delta: deltas.reservation,
        allocator_attempt_delta: deltas.allocator,
        reserve_call_delta: deltas.reserve_call,
        event_release_exact,
        rejection_before: current_peak_evidence(before_pair),
        rejection_after: current_peak_evidence(after_pair),
        attempt_histogram: Some(attempt_histogram),
        sixel_storage: None,
    })
}

async fn measure_kitty_replacement_peak(first: &[u8], second: &[u8]) -> Result<u64, String> {
    let policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX);
    let mut measurement = PtyTerminalImageState::new(policy);
    drop(route(&mut measurement, first.to_vec()).await?);
    let before = counters(&measurement)?;
    let owner = measurement.storage_ownership();
    drop(route(&mut measurement, second.to_vec()).await.map_err(|error| {
        format!(
            "measurement second chunk: {error}; before={before:?}; owner={owner:?}; reconcile={:?}",
            measurement.validation_reconcile_rejection()
        )
    })?);
    let peak = counters(&measurement)?.0.requested_peak;
    measurement.release_retained_storage();
    Ok(peak)
}

fn expected_kitty_sinks(
    bytes: &[u8],
    before: SessionTerminalState,
) -> (ObservedSink, ObservedSink) {
    let rejection = PtyReaderIngressRejection {
        error: SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit),
        image_sequence: before.sequence,
    };
    let common = (bytes.len() as u64, digest(bytes));
    let client = ObservedSink {
        calls: 1,
        bytes: common.0,
        digest: common.1,
        rejection_calls: 1,
        rejection: Some(rejection),
        real_term_feed: false,
        term_cursor_column: 0,
        term_cell: '\0',
        observer_cursor_column: 0,
    };
    let term = ObservedSink {
        calls: 1,
        bytes: common.0,
        digest: common.1,
        rejection_calls: 0,
        rejection: None,
        real_term_feed: true,
        term_cursor_column: 1,
        term_cell: 'A',
        observer_cursor_column: 1,
    };
    (client, term)
}

async fn limited_kitty_session(first: Vec<u8>) -> Result<PtyTerminalImageState, String> {
    let policy = TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        u64::MAX,
        u64::MAX,
        StorageAllocationClass::DecodedKitty,
        2,
        GraphicsStorageRejection::SessionLimit,
    );
    let mut session = PtyTerminalImageState::new(policy);
    let commit = route(&mut session, first).await?;
    if !matches!(
        commit.outputs.as_slice(),
        [SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Kitty { .. }, .. }]
    ) {
        return Err(format!("first Kitty chunk did not use production boundary: {commit:?}"));
    }
    drop(commit);
    Ok(session)
}

fn kitty_attempt_histogram(
    session: AttemptDeltas,
    process: AttemptDeltas,
    rejection: (u64, u64, u64),
) -> Result<KittyAttemptHistogram, String> {
    let stages = vec![
        StorageClassStageEvidence {
            class: "framing_candidate",
            reservations: 2,
            allocator_attempts: 2,
            reserve_before_allocation_calls: 2,
            reconciliations: 2,
            reserve_only_checks: 0,
        },
        StorageClassStageEvidence {
            class: "framing_active",
            reservations: 4,
            allocator_attempts: 3,
            reserve_before_allocation_calls: 4,
            reconciliations: 3,
            reserve_only_checks: 1,
        },
        StorageClassStageEvidence {
            class: "framing_events",
            reservations: 2,
            allocator_attempts: 1,
            reserve_before_allocation_calls: 2,
            reconciliations: 1,
            reserve_only_checks: 1,
        },
        StorageClassStageEvidence {
            class: "terminal_outputs",
            reservations: 2,
            allocator_attempts: 1,
            reserve_before_allocation_calls: 2,
            reconciliations: 1,
            reserve_only_checks: 1,
        },
    ];
    let sum = stages.iter().fold(
        AttemptDeltas { reservation: 0, allocator: 0, reserve_call: 0, reconcile: 0 },
        |sum, stage| AttemptDeltas {
            reservation: sum.reservation.saturating_add(stage.reservations),
            allocator: sum.allocator.saturating_add(stage.allocator_attempts),
            reserve_call: sum.reserve_call.saturating_add(stage.reserve_before_allocation_calls),
            reconcile: sum.reconcile.saturating_add(stage.reconciliations),
        },
    );
    if session != sum || process != sum || rejection != (2, 1, 0) {
        return Err(format!(
            "Kitty attempt histogram drifted: session={session:?} process={process:?} \
             expected={sum:?} targeted={rejection:?}"
        ));
    }
    Ok(KittyAttemptHistogram {
        stages,
        reserve_only_checks: [
            "framing_active_empty_owner",
            "framing_events_empty_vector",
            "terminal_outputs_empty_vector",
        ],
        targeted_failure_class: "decoded_kitty",
        targeted_failure_occurrence: 2,
        matching_reservations: rejection.0,
        fired_rejections: rejection.1,
        staged_allocations: rejection.2,
        global_max_minus_scope: "first_ingress_framing_peak",
        final_rollback_scope: "decoded_kitty_occurrence_2",
        session,
        process,
    })
}

async fn verify_completed_kitty() -> Result<(usize, usize), String> {
    let first_chunk = b"\x1b_Gf=32,s=2,v=1,m=1;/wAA\x1b\\";
    let final_chunk = b"\x1b_Gm=0;/wD/AP8=\x1b\\";
    let policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX);
    let mut session = PtyTerminalImageState::new(policy);
    let first_result = route(&mut session, first_chunk.to_vec()).await;
    drop(first_result.map_err(|error| {
        completed_kitty_context("unbounded first", u64::MAX, "none", &session, &error)
    })?);
    let final_result = route(&mut session, final_chunk.to_vec()).await;
    drop(final_result.map_err(|error| {
        completed_kitty_context("unbounded final", u64::MAX, "none", &session, &error)
    })?);
    let owner = session.storage_ownership();
    let digests = session.validation_storage_digests();
    let measured =
        counters(&session).map_err(|error| format!("unbounded counters after final: {error}"))?.0;
    let expected_rgba = [255, 0, 0, 255, 0, 255, 0, 255];
    if measured.requested_peak != 1_043
        || owner.pending_kitty_requested != 0
        || owner.completed_kitty_requested != 0
        || owner.completed_kitty_observed != 0
        || owner.kitty_decoded_requested != 8
        || owner.kitty_decoded_observed < owner.kitty_decoded_requested
        || digests.kitty_decoded != digest(&expected_rgba)
        || measured.requested_current != 8
        || measured.observed_current != 8
    {
        return Err(format!(
            "completed Kitty ownership drifted: {owner:?} measured={measured:?} digests={digests:?}"
        ));
    }
    session.release_retained_storage();
    if counters(&session)
        .map_err(|error| format!("unbounded counters after release: {error}"))?
        .0
        .requested_current
        != 0
    {
        return Err("completed Kitty storage did not release to zero".to_owned());
    }

    verify_completed_kitty_limits(first_chunk, final_chunk, measured).await?;
    Ok((owner.kitty_decoded_requested, owner.kitty_decoded_observed))
}

async fn verify_completed_kitty_limits(
    first_chunk: &[u8],
    final_chunk: &[u8],
    measured: ImageStorageCounters,
) -> Result<(), String> {
    let mut exact =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            measured.requested_peak,
            u64::MAX,
        ));
    let exact_first_result = route(&mut exact, first_chunk.to_vec()).await;
    drop(exact_first_result.map_err(|error| {
        completed_kitty_context("exact first", measured.requested_peak, "none", &exact, &error)
    })?);
    let exact_final_result = route(&mut exact, final_chunk.to_vec()).await;
    drop(exact_final_result.map_err(|error| {
        completed_kitty_context("exact final", measured.requested_peak, "none", &exact, &error)
    })?);
    exact.release_retained_storage();
    if counters(&exact)
        .map_err(|error| format!("exact counters after release: {error}"))?
        .0
        .requested_current
        != 0
    {
        return Err("exact split Kitty storage did not release to zero".to_owned());
    }

    let mut rejected =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            measured.requested_peak.saturating_sub(1),
            u64::MAX,
        ));
    let rejected_limit = measured.requested_peak.saturating_sub(1);
    let before_counters =
        counters(&rejected).map_err(|error| format!("max-minus counters before first: {error}"))?;
    let before_owner = rejected.storage_ownership();
    let before_state = rejected.state();
    let before_digests = rejected.validation_storage_digests();
    let rejection = rejected.process_bytes(first_chunk);
    let after_counters =
        counters(&rejected).map_err(|error| format!("max-minus counters after first: {error}"))?;
    let expected_attempts = completed_kitty_rejection_counters();
    if !matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) || before_counters.0 != ImageStorageCounters::default()
        || before_counters.1 != ImageStorageCounters::default()
        || after_counters.0 != expected_attempts
        || after_counters.1 != expected_attempts
        || before_owner != rejected.storage_ownership()
        || before_state != rejected.state()
        || before_digests != rejected.validation_storage_digests()
        || rejected.validation_rejection_observation() != (0, 0, 0)
    {
        return Err(format!(
            "split Kitty global max-minus-one rollback drifted: limit={rejected_limit} \
             result={rejection:?} owner={before_owner:?}->{:?} state={before_state:?}->{:?} \
             digests={before_digests:?}->{:?} counters={before_counters:?}->{after_counters:?} \
             target={:?}",
            rejected.storage_ownership(),
            rejected.state(),
            rejected.validation_storage_digests(),
            rejected.validation_rejection_observation(),
        ));
    }
    rejected.release_retained_storage();
    if counters(&rejected)
        .map_err(|error| format!("max-minus counters after release: {error}"))?
        .0
        .requested_current
        != 0
    {
        return Err("rejected split Kitty storage did not release to zero".to_owned());
    }
    Ok(())
}

fn completed_kitty_rejection_counters() -> ImageStorageCounters {
    ImageStorageCounters {
        requested_current: 0,
        requested_peak: 0,
        observed_current: 0,
        observed_peak: 0,
        reservation_attempts: 12,
        allocator_attempts: 9,
        reserve_before_allocation_calls: 12,
        observed_reconciliations: 9,
    }
}

fn completed_kitty_context(
    operation: &str,
    session_limit: u64,
    allocation_target: &str,
    session: &PtyTerminalImageState,
    error: &str,
) -> String {
    format!(
        "completed Kitty {operation}: error={error} session_limit={session_limit} \
         process_limit={} allocation_target={allocation_target} owner={:?} counters={:?} \
         rejection_telemetry={:?}",
        u64::MAX,
        session.storage_ownership(),
        session.validation_storage_counters(),
        session.validation_rejection_observation(),
    )
}

async fn verify_sixel_paths() -> Result<ProtocolEvidence, String> {
    let (retained, peak, released, mut storage_evidence) = verify_exact_sixel_storage().await?;
    let mut rejected_session =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            peak.requested_peak.saturating_sub(1),
            u64::MAX,
        ));
    let before_pair = counters(&rejected_session)?;
    let before_owner = rejected_session.storage_ownership();
    let before_state = rejected_session.state();
    let before_digests = rejected_session.validation_storage_digests();
    let rejected_bytes = b"A\x1bPq????\x1b\\".to_vec();
    let (rejected, client, term) =
        route_observed(&mut rejected_session, rejected_bytes.clone()).await;
    let (delivery, matching_digest) = delivery_checks(&rejected_bytes, client, term);
    let DeliveryEvidence { client_delivery_once, term_feed_once } = delivery;
    let after_pair = counters(&rejected_session)?;
    let deltas = attempt_deltas("Sixel", before_pair.0, after_pair.0)?;
    let process_deltas = attempt_deltas("Sixel process", before_pair.1, after_pair.1)?;
    let event_release_exact = ownership_counters_equal(before_pair, after_pair);
    let zero_class_state = sixel_classes_released(&rejected_session)?;
    let rollback = matches!(
        &rejected,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) && before_owner == rejected_session.storage_ownership()
        && before_state == rejected_session.state()
        && before_digests == rejected_session.validation_storage_digests()
        && event_release_exact
        && zero_class_state
        && deltas
            == AttemptDeltas { reservation: 12, allocator: 9, reserve_call: 12, reconcile: 9 }
        && process_deltas == deltas
        && rejected_session.validation_reconcile_rejection().is_none()
        && rejected_session.validation_rejection_observation() == (0, 0, 0);
    if !rollback {
        return Err(format!(
            "Sixel global 791 rejection measurement: result={rejected:?} \
             stage=terminal_outputs_publication_reserve \
             owner={before_owner:?}->{:?} state={before_state:?}->{:?} \
             digests={before_digests:?}->{:?} counters={before_pair:?}->{after_pair:?} \
             deltas={deltas:?} target={:?}",
            rejected_session.storage_ownership(),
            rejected_session.state(),
            rejected_session.validation_storage_digests(),
            rejected_session.validation_rejection_observation(),
        ));
    }
    set_sixel_rejection_evidence(&mut storage_evidence);
    rejected_session.release_retained_storage();
    let rejected_released = counters(&rejected_session)?.0;
    if !sixel_release_valid(rejected_released, &delivery, matching_digest, client) {
        return Err(format!(
            "Sixel retention/release drifted: {rejected:?} {retained:?} {released:?}"
        ));
    }
    Ok(ProtocolEvidence {
        retained_requested: retained.sixel_body_requested,
        retained_observed: retained.sixel_body_observed,
        completed_requested: retained.sixel_body_requested,
        completed_observed: retained.sixel_body_observed,
        replacement_peak: peak.requested_peak,
        typed_rejection: "session_limit",
        rollback,
        current_after_release: released.requested_current + rejected_released.requested_current,
        storage_error: "session_limit",
        routing: routing_evidence(
            DeliveryEvidence { client_delivery_once, term_feed_once },
            matching_digest,
            client,
        ),
        reservation_attempt_delta: deltas.reservation,
        allocator_attempt_delta: deltas.allocator,
        reserve_call_delta: deltas.reserve_call,
        event_release_exact,
        rejection_before: current_peak_evidence(before_pair),
        rejection_after: current_peak_evidence(after_pair),
        attempt_histogram: None,
        sixel_storage: Some(storage_evidence),
    })
}

fn sixel_classes_released(session: &PtyTerminalImageState) -> Result<bool, String> {
    Ok(sixel_class_decomposition(session)?.iter().all(|(_, (session, process))| {
        *session == ImageStorageClassCounters::default()
            && *process == ImageStorageClassCounters::default()
    }))
}

fn set_sixel_rejection_evidence(evidence: &mut SixelStorageEvidence) {
    "terminal_outputs_publication_reserve".clone_into(&mut evidence.global_max_minus_stage);
    evidence.global_max_minus_overlap = [528, 288, 4, 4];
}

fn sixel_release_valid(
    released: ImageStorageCounters,
    delivery: &DeliveryEvidence,
    matching_digest: bool,
    client: ObservedSink,
) -> bool {
    released.requested_current == 0
        && delivery.client_delivery_once
        && delivery.term_feed_once
        && matching_digest
        && client.rejection_calls == 1
        && client.rejection.is_some()
}

async fn verify_exact_sixel_storage() -> Result<
    (ImageStorageOwnership, ImageStorageCounters, ImageStorageCounters, SixelStorageEvidence),
    String,
> {
    let bytes = b"A\x1bPq????\x1b\\".to_vec();
    let mut measurement = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let measured_commit = route(&mut measurement, bytes.clone()).await?;
    drop(measured_commit);
    let measured_peak = counters(&measurement)?.0.requested_peak;
    measurement.release_retained_storage();

    let policy =
        TerminalImageProcessPolicy::with_storage_limits_for_validation(measured_peak, u64::MAX);
    let mut session = PtyTerminalImageState::new(policy);
    let completed = route(&mut session, bytes).await?;
    if !matches!(
        completed.outputs.last(),
        Some(SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Sixel { .. }, .. })
    ) {
        return Err(format!("Sixel body did not use production boundary: {completed:?}"));
    }
    drop(completed);
    let retained = session.storage_ownership();
    let (peak, process_peak) = counters(&session)?;
    let digests = session.validation_storage_digests();
    let class_decomposition = sixel_class_decomposition(&session)?;
    let class_exact = class_decomposition.iter().all(|(name, (session_class, process_class))| {
        session_class == process_class
            && match *name {
                "candidate" => storage_class_exact(*session_class, 0, 3),
                "active" => storage_class_exact(*session_class, 0, 6),
                "events" => storage_class_exact(*session_class, 0, 480),
                "outputs" => storage_class_exact(*session_class, 0, 528),
                "canonical_sixel" => storage_class_exact(*session_class, 4, 4),
                "decoded_sixel" => storage_class_exact(*session_class, 96, 288),
                _ => false,
            }
    });
    if !exact_sixel_values(retained, peak, process_peak, digests, class_exact) {
        return Err(format!(
            "exact Sixel production accounting drifted: retained={retained:?} counters={peak:?} \
             classes={class_decomposition:?} dimensions=4x6 decoded_growth=48+192 \
             decoded_compaction=192+96 observed_capacities=48,192,96 digests={digests:?}"
        ));
    }
    session.release_retained_storage();
    let (released, _) = counters(&session)?;
    if released.requested_current != 0 || released.observed_current != 0 {
        return Err(format!("exact Sixel release drifted: {released:?}"));
    }

    let classes = class_decomposition
        .into_iter()
        .map(|(class, (session_class, _))| StorageClassCapacityEvidence {
            class,
            requested_current: session_class.requested_current,
            requested_peak: session_class.requested_peak,
            observed_current: session_class.observed_current,
            observed_peak: session_class.observed_peak,
        })
        .collect();
    Ok((
        retained,
        peak,
        released,
        SixelStorageEvidence {
            dimensions: [4, 6],
            classes,
            decoded_capacities: [48, 192, 96],
            decoded_growth_overlap: 240,
            decoded_compaction_overlap: 288,
            body_digest: digests.sixel_body,
            decoded_digest: digests.sixel_decoded,
            exact_limit: measured_peak,
            session_telemetry: storage_telemetry(peak),
            process_telemetry: storage_telemetry(process_peak),
            global_max_minus_stage: String::new(),
            global_max_minus_overlap: [0; 4],
        },
    ))
}

fn exact_sixel_values(
    retained: ImageStorageOwnership,
    peak: ImageStorageCounters,
    process_peak: ImageStorageCounters,
    digests: ImageStorageDigests,
    class_exact: bool,
) -> bool {
    retained.sixel_body_requested == 4
        && retained.sixel_body_observed == 4
        && retained.sixel_decoded_requested == 96
        && retained.sixel_decoded_observed == 96
        && class_exact
        && peak.requested_current == 100
        && peak.observed_current == 100
        && peak.requested_peak == 1_304
        && peak.observed_peak == 1_304
        && peak.reservation_attempts == 13
        && peak.allocator_attempts == 10
        && peak.reserve_before_allocation_calls == 13
        && peak.observed_reconciliations == 10
        && process_peak == peak
        && digests.sixel_body == 2_489_256_947_087_179_384
        && digests.sixel_decoded == 13_492_316_921_505_547_432
}

fn verify_ledger_atomicity() -> Result<LedgerAtomicityEvidence, String> {
    let requested_counter_overflow = verify_unchanged_fault(
        StorageLedgerValidationFault::RequestedCounterOverflow,
        0,
        GraphicsStorageRejection::CounterOverflow,
    )?;
    let observed_counter_overflow = verify_unchanged_fault(
        StorageLedgerValidationFault::ObservedCounterOverflow,
        0,
        GraphicsStorageRejection::CounterOverflow,
    )?;
    let reservation_counter_overflow = verify_unchanged_fault(
        StorageLedgerValidationFault::ReservationAttemptOverflow,
        0,
        GraphicsStorageRejection::CounterOverflow,
    )?;
    let reserve_call_overflow = verify_unchanged_fault(
        StorageLedgerValidationFault::ReserveCallOverflow,
        0,
        GraphicsStorageRejection::CounterOverflow,
    )?;
    let poisoned_ledger = verify_unchanged_fault(
        StorageLedgerValidationFault::Poisoned,
        0,
        GraphicsStorageRejection::InternalInvariant,
    )?;

    let allocation_counter_overflow = verify_allocator_counter_overflow()?;
    let reconciliation_counter_overflow = verify_reconciliation_counter_overflow()?;
    let mixed_rejections_unchanged = verify_mixed_snapshot_precedence_cases()?;

    let paired_partial_charge_prevented = requested_counter_overflow
        && observed_counter_overflow
        && reservation_counter_overflow
        && reserve_call_overflow
        && allocation_counter_overflow
        && reconciliation_counter_overflow
        && poisoned_ledger
        && mixed_rejections_unchanged;
    if !paired_partial_charge_prevented {
        return Err("paired ledger fault rollback failed".to_owned());
    }
    Ok(LedgerAtomicityEvidence {
        requested_counter_overflow: "pass",
        observed_counter_overflow: "pass",
        reservation_counter_overflow: "pass",
        allocation_counter_overflow: "pass",
        reconciliation_counter_overflow: "pass",
        poisoned_ledger: "pass",
        paired_partial_charge_prevented,
        reserve_mixed_precedence: "internal_before_capacity_both_orderings",
        reconcile_mixed_precedence: "internal_before_capacity_both_orderings",
        mixed_rejections_unchanged,
    })
}

fn verify_mixed_snapshot_precedence_cases() -> Result<bool, String> {
    let cases = [
        mixed_snapshot_case(
            (64, 0),
            0,
            StorageSnapshotValidationFault {
                operation: StorageLedgerOperation::Reserve,
                scope: StorageLedgerScope::Session,
                rejection: GraphicsStorageRejection::CounterOverflow,
            },
        ),
        mixed_snapshot_case(
            (0, 64),
            0,
            StorageSnapshotValidationFault {
                operation: StorageLedgerOperation::Reserve,
                scope: StorageLedgerScope::Process,
                rejection: GraphicsStorageRejection::InternalInvariant,
            },
        ),
        mixed_snapshot_case(
            (64, 1),
            1,
            StorageSnapshotValidationFault {
                operation: StorageLedgerOperation::Reconcile,
                scope: StorageLedgerScope::Session,
                rejection: GraphicsStorageRejection::CounterOverflow,
            },
        ),
        mixed_snapshot_case(
            (1, 64),
            1,
            StorageSnapshotValidationFault {
                operation: StorageLedgerOperation::Reconcile,
                scope: StorageLedgerScope::Process,
                rejection: GraphicsStorageRejection::InternalInvariant,
            },
        ),
    ];
    for case in cases {
        verify_mixed_snapshot_precedence(case)?;
    }
    Ok(true)
}

fn mixed_snapshot_case(
    limits: (u64, u64),
    observed_capacity_extra: usize,
    fault: StorageSnapshotValidationFault,
) -> MixedSnapshotCase {
    MixedSnapshotCase {
        session_limit: limits.0,
        process_limit: limits.1,
        observed_capacity_extra,
        fault,
    }
}

fn verify_mixed_snapshot_precedence(case: MixedSnapshotCase) -> Result<(), String> {
    let policy = TerminalImageProcessPolicy::with_storage_snapshot_fault_for_validation(
        case.session_limit,
        case.process_limit,
        case.observed_capacity_extra,
        case.fault,
    );
    let mut session = PtyTerminalImageState::new(policy);
    let before_counters = session.validation_storage_counters();
    let before_state = session.state();
    let before_owner = session.storage_ownership();
    let before_digests = session.validation_storage_digests();
    let result = session.process_bytes(&kitty_rgba(1, 1, &[1, 2, 3, 4]));
    let after_counters = session.validation_storage_counters();
    let expected_delta = match case.fault.operation {
        StorageLedgerOperation::Reserve => {
            AttemptDeltas { reservation: 0, allocator: 0, reserve_call: 0, reconcile: 0 }
        }
        StorageLedgerOperation::Reconcile => {
            AttemptDeltas { reservation: 3, allocator: 1, reserve_call: 3, reconcile: 0 }
        }
    };
    let telemetry_exact = attempt_deltas("mixed session", before_counters.0, after_counters.0)?
        == expected_delta
        && attempt_deltas("mixed process", before_counters.1, after_counters.1)? == expected_delta;
    let classes_zero = [
        StorageAllocationClass::FramingCandidate,
        StorageAllocationClass::FramingActive,
        StorageAllocationClass::FramingEvents,
        StorageAllocationClass::TerminalOutputs,
        StorageAllocationClass::CanonicalSixel,
        StorageAllocationClass::DecodedKitty,
        StorageAllocationClass::DecodedSixel,
        StorageAllocationClass::GridObservations,
    ]
    .into_iter()
    .all(|class| {
        session.validation_storage_class_counters(class).is_ok_and(|(session, process)| {
            session == ImageStorageClassCounters::default()
                && process == ImageStorageClassCounters::default()
        })
    });
    let unchanged = storage_state_equal(before_counters, after_counters)
        && telemetry_exact
        && classes_zero
        && before_state == session.state()
        && before_owner == session.storage_ownership()
        && before_digests == session.validation_storage_digests();
    let typed = matches!(
        result,
        Err(SessionTerminalError::Storage(actual)) if actual == case.fault.rejection
    );
    if !typed || !unchanged {
        return Err(format!(
            "mixed {:?}/{:?} precedence drifted: {result:?} \
             counters={before_counters:?}->{after_counters:?} expected_delta={expected_delta:?} \
             classes_zero={classes_zero} owner={before_owner:?}->{:?} state={before_state:?}->{:?}",
            case.fault.operation,
            case.fault.scope,
            session.storage_ownership(),
            session.state(),
        ));
    }
    Ok(())
}

fn verify_allocator_counter_overflow() -> Result<bool, String> {
    let policy = TerminalImageProcessPolicy::with_storage_fault_for_validation(
        64,
        128,
        0,
        StorageLedgerValidationFault::AllocatorAttemptOverflow,
    );
    let mut allocator = PtyTerminalImageState::new(policy);
    let allocator_before = allocator.validation_storage_counters();
    let allocator_result = allocator.process_bytes(&kitty_rgba(1, 1, &[1, 2, 3, 4]));
    let allocator_after = allocator.validation_storage_counters();
    let allocation_counter_overflow = matches!(
        allocator_result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::CounterOverflow))
    ) && allocator_after.0.requested_current == 0
        && allocator_after.0.observed_current == 0
        && allocator_after.1.requested_current == 0
        && allocator_after.1.observed_current == 0
        && allocator_after.0.allocator_attempts == allocator_before.0.allocator_attempts
        && allocator_after.1.allocator_attempts == allocator_before.1.allocator_attempts;

    if !allocation_counter_overflow {
        return Err(format!(
            "allocator counter rollback failed: {allocator_result:?} {allocator_before:?}->{allocator_after:?}"
        ));
    }
    Ok(true)
}

fn verify_reconciliation_counter_overflow() -> Result<bool, String> {
    let policy = TerminalImageProcessPolicy::with_storage_fault_for_validation(
        64,
        128,
        1,
        StorageLedgerValidationFault::ReconciliationOverflow,
    );
    let mut reconciliation = PtyTerminalImageState::new(policy);
    let reconciliation_before = reconciliation.validation_storage_counters();
    let reconciliation_result = reconciliation.process_bytes(b"\x1bPq~\x1b\\");
    let reconciliation_after = reconciliation.validation_storage_counters();
    let reconciliation_counter_overflow = matches!(
        reconciliation_result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::CounterOverflow))
    ) && reconciliation_after.0.requested_current == 0
        && reconciliation_after.0.observed_current == 0
        && reconciliation_after.1.requested_current == 0
        && reconciliation_after.1.observed_current == 0
        && reconciliation_after.0.observed_reconciliations
            == reconciliation_before.0.observed_reconciliations
        && reconciliation_after.1.observed_reconciliations
            == reconciliation_before.1.observed_reconciliations;

    if !reconciliation_counter_overflow {
        return Err(format!(
            "reconciliation counter rollback failed: {reconciliation_result:?} {reconciliation_before:?}->{reconciliation_after:?}"
        ));
    }
    Ok(true)
}

fn verify_unchanged_fault(
    fault: StorageLedgerValidationFault,
    observed_capacity_extra: usize,
    expected: GraphicsStorageRejection,
) -> Result<bool, String> {
    let policy = TerminalImageProcessPolicy::with_storage_fault_for_validation(
        u64::MAX,
        u64::MAX,
        observed_capacity_extra,
        fault,
    );
    let mut session = PtyTerminalImageState::new(policy);
    let before = session.validation_storage_counters();
    let result = session.process_bytes(&kitty_rgba(1, 1, &[1, 2, 3, 4]));
    let after = session.validation_storage_counters();
    let mut expected_after = before;
    let (stage, expected_reconcile) = match fault {
        StorageLedgerValidationFault::RequestedCounterOverflow => {
            for counters in [&mut expected_after.0, &mut expected_after.1] {
                counters.reservation_attempts = counters.reservation_attempts.saturating_add(2);
                counters.reserve_before_allocation_calls =
                    counters.reserve_before_allocation_calls.saturating_add(2);
            }
            ("candidate_reserve_after_outputs_and_events_empty", None)
        }
        StorageLedgerValidationFault::ObservedCounterOverflow => {
            for counters in [&mut expected_after.0, &mut expected_after.1] {
                counters.reservation_attempts = counters.reservation_attempts.saturating_add(2);
                counters.reserve_before_allocation_calls =
                    counters.reserve_before_allocation_calls.saturating_add(2);
            }
            ("candidate_reserve_observed_ceiling_after_outputs_and_events_empty", None)
        }
        StorageLedgerValidationFault::ReservationAttemptOverflow => {
            ("terminal_outputs_empty_reserve_attempt", None)
        }
        StorageLedgerValidationFault::ReserveCallOverflow => {
            ("terminal_outputs_empty_reserve_before_allocation", None)
        }
        StorageLedgerValidationFault::Poisoned => ("terminal_outputs_health_gate", None),
        StorageLedgerValidationFault::AllocatorAttemptOverflow
        | StorageLedgerValidationFault::ReconciliationOverflow => {
            return Err(format!("unexpected ledger fault routed through reserve probe: {fault:?}"));
        }
    };
    let (classes_valid, class_trace) = unchanged_fault_classes(&session, fault);
    let valid = matches!(result, Err(SessionTerminalError::Storage(actual)) if actual == expected)
        && storage_state_equal(before, after)
        && after == expected_after
        && classes_valid
        && session.storage_ownership() == ImageStorageOwnership::default()
        && session.state().sequence.0 == 0
        && session.validation_rejection_observation() == (0, 0, 0)
        && session.validation_reconcile_rejection() == expected_reconcile;
    if !valid {
        return Err(format!(
            "ledger fault {fault:?} changed paired state at {stage}: result={result:?} \
             counters={before:?}->{after:?} expected={expected_after:?} \
             classes_valid={classes_valid} class_results={class_trace} \
             reconcile={:?}/{expected_reconcile:?} owner={:?} state={:?}",
            session.validation_reconcile_rejection(),
            session.storage_ownership(),
            session.state(),
        ));
    }
    Ok(true)
}

fn unchanged_fault_classes(
    session: &PtyTerminalImageState,
    fault: StorageLedgerValidationFault,
) -> (bool, String) {
    let results = [
        StorageAllocationClass::FramingCandidate,
        StorageAllocationClass::FramingActive,
        StorageAllocationClass::FramingEvents,
        StorageAllocationClass::TerminalOutputs,
        StorageAllocationClass::CanonicalSixel,
        StorageAllocationClass::DecodedKitty,
        StorageAllocationClass::DecodedSixel,
    ]
    .into_iter()
    .map(|class| session.validation_storage_class_counters(class))
    .collect::<Vec<_>>();
    let valid = if fault == StorageLedgerValidationFault::Poisoned {
        results.len() == 7
            && results.iter().all(|result| {
                matches!(
                    result,
                    Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))
                )
            })
    } else {
        results.iter().all(|result| {
            result.is_ok_and(|(session_class, process_class)| {
                session_class == ImageStorageClassCounters::default()
                    && process_class == ImageStorageClassCounters::default()
            })
        })
    };
    (valid, format!("{results:?}"))
}

fn storage_state_equal(
    before: (ImageStorageCounters, ImageStorageCounters),
    after: (ImageStorageCounters, ImageStorageCounters),
) -> bool {
    [(before.0, after.0), (before.1, after.1)].into_iter().all(|(before, after)| {
        before.requested_current == after.requested_current
            && before.requested_peak == after.requested_peak
            && before.observed_current == after.observed_current
            && before.observed_peak == after.observed_peak
    })
}

async fn verify_observed_capacity() -> Result<ObservedCapacityEvidence, String> {
    let (framer_requested, framer_observed) = verify_framer_observed_capacity().await?;
    let bytes = kitty_rgba(1, 1, &[1, 2, 3, 4]);
    let success_counters = verify_decoder_observed_success(&bytes)?;
    verify_failed_sixel_reconcile(
        framer_requested,
        framer_observed,
        success_counters,
        success_counters.observed_peak.saturating_sub(1),
        &bytes,
    )
}

fn verify_decoder_observed_success(bytes: &[u8]) -> Result<ImageStorageCounters, String> {
    let success_policy = observed_capacity_policy(u64::MAX, 3);
    let mut success = PtyTerminalImageState::new(success_policy);
    drop(success.process_bytes(bytes).map_err(|error| {
        observed_capacity_context(
            "decoder success",
            ObservedCapacityLimits { session: u64::MAX, process: u64::MAX, extra: 3 },
            &success,
            &error.to_string(),
        )
    })?);
    let success_pair = counters(&success)?;
    let success_counters = success_pair.0;
    let success_owner = success.storage_ownership();
    if success_counters.observed_current <= success_counters.requested_current
        || success_counters.observed_reconciliations == 0
        || success_owner.kitty_decoded_observed <= success_owner.kitty_decoded_requested
    {
        return Err(format!(
            "extra observed capacity was not reconciled: {success_counters:?} {success_owner:?}"
        ));
    }
    let success_classes = [
        StorageAllocationClass::FramingCandidate,
        StorageAllocationClass::FramingActive,
        StorageAllocationClass::FramingEvents,
        StorageAllocationClass::TerminalOutputs,
        StorageAllocationClass::DecodedKitty,
    ]
    .into_iter()
    .map(|class| (class, success.validation_storage_class_counters(class)))
    .collect::<Vec<_>>();
    let expected_classes = [
        (StorageAllocationClass::FramingCandidate, (0, 3, 0, 9)),
        (StorageAllocationClass::FramingActive, (0, 48, 0, 54)),
        (StorageAllocationClass::FramingEvents, (0, 480, 0, 483)),
        (StorageAllocationClass::TerminalOutputs, (0, 528, 0, 531)),
        (StorageAllocationClass::DecodedKitty, (4, 12, 7, 21)),
    ];
    let classes_exact = success_classes.iter().zip(expected_classes).all(
        |((actual_class, result), (expected_class, values))| {
            *actual_class == expected_class
                && result.is_ok_and(|(session, process)| {
                    session == process
                        && (
                            session.requested_current,
                            session.requested_peak,
                            session.observed_current,
                            session.observed_peak,
                        ) == values
                })
        },
    );
    let success_exact = success_pair.0 == success_pair.1
        && success_counters.requested_current == 4
        && success_counters.requested_peak == 1_048
        && success_counters.observed_current == 7
        && success_counters.observed_peak == 1_063
        && success_counters.reservation_attempts == 15
        && success_counters.allocator_attempts == 12
        && success_counters.reserve_before_allocation_calls == 15
        && success_counters.observed_reconciliations == 12
        && classes_exact
        && success_owner.kitty_decoded_requested == 4
        && success_owner.kitty_decoded_observed == 7
        && success.validation_storage_digests().kitty_decoded == 626_081_712_147_647_002
        && success.state().sequence.0 == 1;
    if !success_exact {
        return Err(format!(
            "decoder observed-capacity exact1031 drifted: owner={success_owner:?} digests={:?} \
             counters={success_pair:?} classes={success_classes:?} state={:?}",
            success.validation_storage_digests(),
            success.state(),
        ));
    }
    success.release_retained_storage();
    if counters(&success)?.0.requested_current != 0 || counters(&success)?.0.observed_current != 0 {
        return Err("successful observed-capacity storage did not release".to_owned());
    }

    Ok(success_counters)
}

fn observed_capacity_policy(session_limit: u64, extra: usize) -> Arc<TerminalImageProcessPolicy> {
    TerminalImageProcessPolicy::with_storage_capacity_observer_for_validation(
        session_limit,
        u64::MAX,
        extra,
    )
}

async fn verify_framer_observed_capacity() -> Result<(usize, usize), String> {
    let bytes = b"\x1b_Gf=32,s=1,v=1,m=1;AAAA\x1b\\".to_vec();
    let measured = measure_framer_observed(&bytes).await?;
    verify_exact_framer_observed(&bytes).await?;
    verify_rejected_framer_observed(&bytes)?;
    Ok(measured)
}

async fn measure_framer_observed(bytes: &[u8]) -> Result<(usize, usize), String> {
    let framer_policy = TerminalImageProcessPolicy::with_storage_capacity_observer_for_validation(
        u64::MAX,
        u64::MAX,
        1,
    );
    let mut framer = PtyTerminalImageState::new(framer_policy);
    let result = route(&mut framer, bytes.to_vec()).await;
    let framer_commit = result.map_err(|error| {
        observed_capacity_context(
            "framer measurement",
            ObservedCapacityLimits { session: u64::MAX, process: u64::MAX, extra: 1 },
            &framer,
            &error,
        )
    })?;
    let (framer_requested, framer_observed) = match framer_commit.outputs.as_slice() {
        [
            SessionTerminalOutput::Image {
                boundary: TerminalImageBoundary::Kitty { command, .. },
                ..
            },
        ] => (command.retained_requested_bytes(), command.retained_observed_bytes()),
        other => return Err(format!("capacity-observed framer boundary drifted: {other:?}")),
    };
    if framer_requested != 32
        || framer_observed != 33
        || !observed_framer_exact(&framer)
        || framer_commit.outputs.len() != 1
        || framer.state().sequence.0 != 1
    {
        return Err(format!(
            "framer observed-capacity exact decomposition drifted: retained={framer_requested}/\
             {framer_observed} owner={:?} counters={:?} state={:?}",
            framer.storage_ownership(),
            framer.validation_storage_counters(),
            framer.state(),
        ));
    }
    drop(framer_commit);
    let framer_owner = framer.storage_ownership();
    if framer_owner.pending_kitty_requested != 3
        || framer_owner.pending_kitty_observed != 4
        || framer_owner.kitty_decoded_requested != 0
    {
        return Err(format!("pending Kitty capacity did not use observer: {framer_owner:?}"));
    }
    framer.release_retained_storage();
    let framer_released = counters(&framer)?.0;
    if framer_released.requested_current != 0 || framer_released.observed_current != 0 {
        return Err("framer observed-capacity storage did not release".to_owned());
    }
    Ok((framer_requested, framer_observed))
}

async fn verify_exact_framer_observed(bytes: &[u8]) -> Result<(), String> {
    let exact_policy = TerminalImageProcessPolicy::with_storage_capacity_observer_for_validation(
        1_047,
        u64::MAX,
        1,
    );
    let mut exact = PtyTerminalImageState::new(exact_policy);
    let exact_commit = route(&mut exact, bytes.to_vec()).await.map_err(|error| {
        observed_capacity_context(
            "framer exact",
            ObservedCapacityLimits { session: 1_047, process: u64::MAX, extra: 1 },
            &exact,
            &error,
        )
    })?;
    if !observed_framer_exact(&exact) || exact_commit.outputs.len() != 1 {
        return Err(format!(
            "framer observed-capacity limit 1047 drifted: owner={:?} counters={:?}",
            exact.storage_ownership(),
            exact.validation_storage_counters(),
        ));
    }
    drop(exact_commit);
    exact.release_retained_storage();
    Ok(())
}

fn verify_rejected_framer_observed(bytes: &[u8]) -> Result<(), String> {
    let rejected_policy = TerminalImageProcessPolicy::with_storage_capacity_observer_for_validation(
        1_046,
        u64::MAX,
        1,
    );
    let mut rejected = PtyTerminalImageState::new(rejected_policy);
    let before = rejected.validation_storage_counters();
    let before_owner = rejected.storage_ownership();
    let before_state = rejected.state();
    let before_digests = rejected.validation_storage_digests();
    let rejection = rejected.process_bytes(bytes);
    let after = rejected.validation_storage_counters();
    let classes = [
        StorageAllocationClass::FramingCandidate,
        StorageAllocationClass::FramingActive,
        StorageAllocationClass::FramingEvents,
        StorageAllocationClass::TerminalOutputs,
        StorageAllocationClass::DecodedKitty,
    ]
    .into_iter()
    .map(|class| (class, rejected.validation_storage_class_counters(class)))
    .collect::<Vec<_>>();
    let expected_after = ImageStorageCounters {
        requested_current: 0,
        requested_peak: 0,
        observed_current: 0,
        observed_peak: 0,
        reservation_attempts: 13,
        allocator_attempts: 10,
        reserve_before_allocation_calls: 13,
        observed_reconciliations: 9,
    };
    let rollback = matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) && before.0 == ImageStorageCounters::default()
        && before.1 == ImageStorageCounters::default()
        && after.0 == expected_after
        && after.1 == expected_after
        && classes.iter().all(|(_, class_result)| {
            class_result.is_ok_and(|(session_class, process_class)| {
                session_class == ImageStorageClassCounters::default()
                    && process_class == ImageStorageClassCounters::default()
            })
        })
        && before_owner == rejected.storage_ownership()
        && before_state == rejected.state()
        && before_digests == rejected.validation_storage_digests()
        && rejected.validation_reconcile_rejection()
            == Some((StorageAllocationClass::TerminalOutputs, 1))
        && rejected.validation_rejection_observation() == (0, 0, 0);
    if !rollback {
        return Err(format!(
            "framer observed-capacity limit 533 drifted: result={rejection:?} reconcile={:?} \
             counters={before:?}->{after:?} classes={classes:?} owner={before_owner:?}->{:?} \
             state={before_state:?}->{:?} digests={before_digests:?}->{:?} target={:?}",
            rejected.validation_reconcile_rejection(),
            rejected.storage_ownership(),
            rejected.state(),
            rejected.validation_storage_digests(),
            rejected.validation_rejection_observation(),
        ));
    }
    Ok(())
}

fn observed_framer_exact(session: &PtyTerminalImageState) -> bool {
    let (session_counters, process_counters) = session.validation_storage_counters();
    let counters_exact = session_counters == process_counters
        && session_counters.requested_current == 563
        && session_counters.requested_peak == 1_043
        && session_counters.observed_current == 566
        && session_counters.observed_peak == 1_047
        && session_counters.reservation_attempts == 13
        && session_counters.allocator_attempts == 10
        && session_counters.reserve_before_allocation_calls == 13
        && session_counters.observed_reconciliations == 10;
    let expected = [
        (StorageAllocationClass::FramingCandidate, (0, 3, 0, 5)),
        (StorageAllocationClass::FramingActive, (32, 48, 33, 50)),
        (StorageAllocationClass::FramingEvents, (0, 480, 0, 481)),
        (StorageAllocationClass::TerminalOutputs, (528, 528, 529, 529)),
        (StorageAllocationClass::DecodedKitty, (3, 3, 4, 4)),
    ];
    let classes_exact = expected.into_iter().all(|(class, values)| {
        session.validation_storage_class_counters(class).is_ok_and(|(session, process)| {
            session == process
                && (
                    session.requested_current,
                    session.requested_peak,
                    session.observed_current,
                    session.observed_peak,
                ) == values
        })
    });
    let owner = session.storage_ownership();
    counters_exact
        && classes_exact
        && owner.pending_kitty_requested == 3
        && owner.pending_kitty_observed == 4
        && owner.kitty_decoded_requested == 0
        && owner.kitty_decoded_observed == 0
}

#[derive(Clone, Copy)]
struct ObservedCapacityLimits {
    session: u64,
    process: u64,
    extra: usize,
}

fn observed_capacity_context(
    operation: &str,
    limits: ObservedCapacityLimits,
    session: &PtyTerminalImageState,
    error: &str,
) -> String {
    format!(
        "observed-capacity {operation}: error={error} session_limit={} \
         process_limit={} observed_extra={} owner={:?} \
         counters={:?} reconcile={:?} target={:?}",
        limits.session,
        limits.process,
        limits.extra,
        session.storage_ownership(),
        session.validation_storage_counters(),
        session.validation_reconcile_rejection(),
        session.validation_rejection_observation(),
    )
}

fn verify_failed_sixel_reconcile(
    framer_requested: usize,
    framer_observed: usize,
    success_counters: ImageStorageCounters,
    failing_limit: u64,
    bytes: &[u8],
) -> Result<ObservedCapacityEvidence, String> {
    let failure_policy = TerminalImageProcessPolicy::with_storage_capacity_observer_for_validation(
        failing_limit,
        u64::MAX,
        3,
    );
    let mut failure = PtyTerminalImageState::new(failure_policy);
    let before = counters(&failure)?;
    let before_owner = failure.storage_ownership();
    let before_state = failure.state();
    let before_digests = failure.validation_storage_digests();
    let result = failure.process_bytes(bytes);
    let after = counters(&failure)?;
    let reconcile_target = failure.validation_reconcile_rejection();
    let failed_reconcile_rollback = matches!(
        result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) && failure.storage_ownership().kitty_decoded_requested == 0
        && ownership_counters_equal(before, after)
        && [after.0, after.1].into_iter().all(|scope| {
            scope.reservation_attempts == 15
                && scope.allocator_attempts == 12
                && scope.reserve_before_allocation_calls == 15
                && scope.observed_reconciliations == 11
                && scope.requested_current == 0
                && scope.requested_peak == 0
                && scope.observed_current == 0
                && scope.observed_peak == 0
        })
        && reconcile_target == Some((StorageAllocationClass::TerminalOutputs, 1))
        && before_owner == failure.storage_ownership()
        && before_state == failure.state()
        && before_digests == failure.validation_storage_digests();
    if !failed_reconcile_rollback {
        return Err(format!(
            "Sixel allocation/reconcile rejection drifted: {result:?} {before:?}->{after:?} \
             target={reconcile_target:?}"
        ));
    }
    Ok(ObservedCapacityEvidence {
        framer_requested,
        framer_observed,
        requested: success_counters.requested_current,
        observed: success_counters.observed_current,
        reconciliations: success_counters.observed_reconciliations,
        failed_reconcile_typed: "session_limit",
        failed_reconcile_rollback,
        failed_reconcile_allocator_attempts_before: before.0.allocator_attempts,
        failed_reconcile_allocator_attempts: after.0.allocator_attempts,
        failed_reconcile_target_class: "terminal_outputs",
        failed_reconcile_target_occurrence: 1,
        failed_reconcile_reservations: after.0.reservation_attempts,
        failed_reconcile_reserve_before: after.0.reserve_before_allocation_calls,
        failed_reconcile_reconciliations: after.0.observed_reconciliations,
    })
}

fn verify_cross_session() -> Result<(CrossSessionEvidence, u64), String> {
    let kitty = kitty_rgba(1, 1, &[1, 2, 3, 4]);
    let sixel = b"\x1bPq~\x1b\\";
    let (process_at_limit, replacement_process) = verify_cross_session_exact(&kitty, sixel)?;
    let rejected_policy =
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, 1_063);
    let mut foreign = PtyTerminalImageState::new(Arc::clone(&rejected_policy));
    let mut pressured = PtyTerminalImageState::new(rejected_policy);
    drop(foreign.process_bytes(&kitty).map_err(|error| error.to_string())?);
    drop(pressured.process_bytes(sixel).map_err(|error| error.to_string())?);
    let before_counters = counters(&pressured)?;
    let first_before = foreign.storage_ownership();
    let second_before = pressured.storage_ownership();
    let foreign_digests = foreign.validation_storage_digests();
    let pressured_digests = pressured.validation_storage_digests();
    let foreign_state = foreign.state();
    let pressured_state = pressured.state();
    let before_classes = sixel_class_decomposition(&pressured)?;
    let rejection = pressured.process_bytes(sixel);
    let typed = matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::ProcessLimit))
    );
    let after_counters = counters(&pressured)?;
    let rejection_classes = sixel_class_decomposition(&pressured)?;
    let unchanged = first_before == foreign.storage_ownership()
        && second_before == pressured.storage_ownership()
        && foreign_digests == foreign.validation_storage_digests()
        && pressured_digests == pressured.validation_storage_digests()
        && foreign_state == foreign.state()
        && pressured_state == pressured.state()
        && before_classes == rejection_classes
        && storage_state_equal(before_counters, after_counters)
        && attempt_deltas("cross-session session", before_counters.0, after_counters.0)?
            == AttemptDeltas { reservation: 10, allocator: 7, reserve_call: 10, reconcile: 7 }
        && attempt_deltas("cross-session process", before_counters.1, after_counters.1)?
            == AttemptDeltas { reservation: 10, allocator: 7, reserve_call: 10, reconcile: 7 }
        && pressured.validation_reconcile_rejection().is_none()
        && pressured.validation_rejection_observation() == (0, 0, 0);
    if !typed || !unchanged {
        return Err(format!(
            "cross-session process pressure drifted: {rejection:?} \
             before={before_counters:?} after={after_counters:?}"
        ));
    }
    foreign.release_retained_storage();
    pressured.release_retained_storage();
    let final_process = counters(&pressured)?.1;
    Ok((
        CrossSessionEvidence {
            process_current_at_limit: process_at_limit.requested_current,
            process_peak: process_at_limit.requested_peak,
            typed_rejection: "process_limit",
            foreign_session_unchanged: unchanged,
            current_after_release: final_process.requested_current,
            required_peak: 1_064,
            enforced_limit: 1_063,
            reservation_attempts: after_counters.1.reservation_attempts,
            allocator_attempts: after_counters.1.allocator_attempts,
            reserve_before_allocation_calls: after_counters.1.reserve_before_allocation_calls,
            setup_reservation_attempts: before_counters.1.reservation_attempts,
            setup_allocator_attempts: before_counters.1.allocator_attempts,
            setup_reserve_before_allocation_calls: before_counters
                .1
                .reserve_before_allocation_calls,
            success_reservation_attempts: replacement_process.reservation_attempts,
            success_allocator_attempts: replacement_process.allocator_attempts,
            success_reserve_before_allocation_calls: replacement_process
                .reserve_before_allocation_calls,
            rejection_reservation_delta: 10,
            rejection_allocator_delta: 7,
            rejection_reserve_before_delta: 10,
        },
        final_process.requested_current,
    ))
}

fn verify_cross_session_exact(
    kitty: &[u8],
    sixel: &[u8],
) -> Result<(ImageStorageCounters, ImageStorageCounters), String> {
    let policy = TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, 1_064);
    let mut first = PtyTerminalImageState::new(Arc::clone(&policy));
    let mut second = PtyTerminalImageState::new(policy);
    drop(first.process_bytes(kitty).map_err(|error| error.to_string())?);
    let first_process = counters(&first)?.1;
    let first_owner = first.storage_ownership();
    let first_digests = first.validation_storage_digests();
    drop(second.process_bytes(sixel).map_err(|error| error.to_string())?);
    let (_, process_at_limit) = counters(&second)?;
    let setup_second_owner = second.storage_ownership();
    let setup_classes = sixel_class_decomposition(&second)?;
    drop(second.process_bytes(sixel).map_err(|error| error.to_string())?);
    let replacement_process = counters(&second)?.1;
    let replacement_classes = sixel_class_decomposition(&second)?;
    let replacement_classes_exact = replacement_classes.iter().all(|(name, (session, process))| {
        let expected = match *name {
            "candidate" => (3, 3),
            "active" => (2, 48),
            "events" => (480, 480),
            "outputs" => (528, 528),
            "canonical_sixel" => (2, 2),
            "decoded_sixel" => (96, 96),
            _ => return false,
        };
        (session.requested_peak, process.requested_peak) == expected
            && session.requested_peak == session.observed_peak
            && process.requested_peak == process.observed_peak
    });
    if first_process.requested_current != 4
        || first_process.requested_peak != 1_048
        || first_process.reservation_attempts != 15
        || first_process.allocator_attempts != 12
        || first_process.reserve_before_allocation_calls != 15
        || first_process.observed_reconciliations != 12
        || process_at_limit.requested_current != 29
        || process_at_limit.requested_peak != 1_048
        || process_at_limit.reservation_attempts != 26
        || process_at_limit.allocator_attempts != 20
        || process_at_limit.reserve_before_allocation_calls != 26
        || process_at_limit.observed_reconciliations != 20
        || replacement_process.requested_current != 29
        || replacement_process.requested_peak != 1_064
        || replacement_process.reservation_attempts != 37
        || replacement_process.allocator_attempts != 28
        || replacement_process.reserve_before_allocation_calls != 37
        || replacement_process.observed_reconciliations != 28
        || !replacement_classes_exact
        || first_owner.completed_kitty_requested != 0
        || first_owner.kitty_decoded_requested != 4
        || first_digests.completed_kitty != 0
        || first_digests.kitty_decoded != 626_081_712_147_647_002
        || setup_second_owner.sixel_body_requested != 1
        || setup_second_owner.sixel_decoded_requested != 24
    {
        return Err(format!(
            "exact cross-session accounting drifted: {process_at_limit:?} \
             {replacement_process:?} {first_owner:?} {setup_second_owner:?} \
             setup_classes={setup_classes:?} replacement_classes={replacement_classes:?}"
        ));
    }
    first.release_retained_storage();
    second.release_retained_storage();
    if counters(&second)?.1.requested_current != 0 {
        return Err("exact cross-session storage did not release".to_owned());
    }
    Ok((process_at_limit, replacement_process))
}

fn verify_concurrent_release() -> Result<ConcurrentReleaseEvidence, String> {
    let (reached, resume, policy) = concurrent_release_policy();
    let mut foreign = PtyTerminalImageState::new(Arc::clone(&policy));
    let pressured = PtyTerminalImageState::new(policy);
    let (detached, detached_requested, detached_outputs_requested, detached_total_requested) =
        seed_concurrent_detached(&mut foreign)?;
    let foreign_owner = foreign.storage_ownership();
    let foreign_digests = foreign.validation_storage_digests();
    let foreign_state = foreign.state();
    let before = counters(&foreign)?.1;
    let before_classes = all_storage_classes(&foreign)?;
    let worker = std::thread::spawn(move || {
        let mut pressured = pressured;
        // A wider Sixel body pushes the paused transaction's provisional
        // ownership above the committed peak, so peak rollback is observable.
        let result = pressured
            .process_bytes(b"\x1bPq~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\x1b\\");
        (pressured, result)
    });

    reached.wait();
    drop(detached);
    let after_external_release = counters(&foreign)?.1;
    let external_classes = all_storage_classes(&foreign)?;
    resume.wait();
    let (mut failed_session, result) =
        worker.join().map_err(|_| "concurrent accounting worker panicked".to_owned())?;
    let (pressured_session, process_after_failure) = counters(&failed_session)?;
    let final_classes = all_storage_classes(&failed_session)?;
    let (external_release_applied, provisional_release_exact, failed_peak_unpublished) =
        concurrent_transition_checks(
            before,
            after_external_release,
            process_after_failure,
            detached_total_requested,
        );
    let check = ConcurrentReleaseCheck {
        result: &result,
        detached_requested,
        detached_outputs_requested,
        detached_total_requested,
        before,
        after_external_release,
        pressured_session,
        process_after_failure,
        external_release_applied: pass_status(external_release_applied),
        provisional_release_exact: pass_status(provisional_release_exact),
        failed_peak_unpublished: pass_status(failed_peak_unpublished),
        foreign_unchanged: pass_status(
            foreign_owner == foreign.storage_ownership()
                && foreign_digests == foreign.validation_storage_digests(),
        ),
        class_states_exact: pass_status(concurrent_classes_exact(
            &before_classes,
            &external_classes,
            &final_classes,
        )),
        failed_sequence: failed_session.state().sequence.0,
    };
    if !concurrent_release_valid(&check) {
        return Err(format!(
            "concurrent release rollback drifted: result={result:?} before={before:?} \
             external={after_external_release:?} session={pressured_session:?} \
             process={process_after_failure:?} detached_body={detached_requested} \
             detached_outputs={detached_outputs_requested} detached_total={detached_total_requested} \
             in_flight_peak={} provisional={} before_classes={before_classes:?} \
             external_classes={external_classes:?} final_classes={final_classes:?} \
             foreign_owner={foreign_owner:?}->{:?} foreign_digests={foreign_digests:?}->{:?} \
             foreign_state={foreign_state:?}->{:?}",
            after_external_release.requested_peak,
            after_external_release
                .requested_current
                .saturating_sub(process_after_failure.requested_current),
            foreign.storage_ownership(),
            foreign.validation_storage_digests(),
            foreign.state(),
        ));
    }
    let final_process_current = release_concurrent_sessions(&mut foreign, &mut failed_session)?;
    Ok(concurrent_release_evidence(&check, final_process_current))
}

fn seed_concurrent_detached(
    foreign: &mut PtyTerminalImageState,
) -> Result<(SessionTerminalCommit, usize, usize, usize), String> {
    let detached = foreign
        .process_bytes(&kitty_rgba(1, 1, &[1, 2, 3, 4]))
        .map_err(|error| format!("seed concurrent detached owner: {error}"))?;
    let requested = match detached.outputs.as_slice() {
        [
            SessionTerminalOutput::Image {
                boundary: TerminalImageBoundary::Kitty { command, .. },
                ..
            },
        ] => command.retained_requested_bytes(),
        other => return Err(format!("detached owner did not use Kitty boundary: {other:?}")),
    };
    let outputs_requested = detached.outputs.requested_bytes();
    let total_requested = requested.saturating_add(outputs_requested);
    Ok((detached, requested, outputs_requested, total_requested))
}

fn concurrent_release_policy() -> (Arc<Barrier>, Arc<Barrier>, Arc<TerminalImageProcessPolicy>) {
    let reached = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let policy = TerminalImageProcessPolicy::with_paused_storage_rejection_for_validation(
        StorageAllocationClass::DecodedSixel,
        1,
        GraphicsStorageRejection::InternalInvariant,
        Arc::clone(&reached),
        Arc::clone(&resume),
    );
    (reached, resume, policy)
}

fn concurrent_transition_checks(
    before: ImageStorageCounters,
    external: ImageStorageCounters,
    failed: ImageStorageCounters,
    detached_total: usize,
) -> (bool, bool, bool) {
    let external_applied = before.requested_current
        == failed
            .requested_current
            .saturating_add(u64::try_from(detached_total).unwrap_or(u64::MAX));
    // Retained candidate, active, canonical, and framing-event ownership the
    // paused transaction still holds until its consuming iterator drops.
    let provisional_exact = external.requested_current >= failed.requested_current
        && external.requested_current - failed.requested_current == 592;
    // The in-flight peak must exceed the committed one, or restoring the
    // committed peak would prove nothing.
    let peak_unpublished = external.requested_peak > before.requested_peak
        && external.observed_peak > before.observed_peak
        && failed.requested_peak == before.requested_peak
        && failed.observed_peak == before.observed_peak;
    (external_applied, provisional_exact, peak_unpublished)
}

fn release_concurrent_sessions(
    foreign: &mut PtyTerminalImageState,
    failed: &mut PtyTerminalImageState,
) -> Result<u64, String> {
    foreign.release_retained_storage();
    failed.release_retained_storage();
    let current = counters(failed)?.1.requested_current;
    if current != 0 {
        return Err(format!("concurrent release final process ownership drifted: {current}"));
    }
    Ok(current)
}

const fn pass_status(passed: bool) -> &'static str {
    if passed { "pass" } else { "fail" }
}

fn concurrent_release_evidence(
    check: &ConcurrentReleaseCheck<'_>,
    final_process_current: u64,
) -> ConcurrentReleaseEvidence {
    ConcurrentReleaseEvidence {
        detached_requested: check.detached_requested,
        detached_outputs_requested: check.detached_outputs_requested,
        detached_total_requested: check.detached_total_requested,
        in_flight_process_peak: check.after_external_release.requested_peak,
        process_current_before: check.before.requested_current,
        process_current_after_external_release: check.after_external_release.requested_current,
        process_current_after_failure: check.process_after_failure.requested_current,
        process_peak_after_failure: check.process_after_failure.requested_peak,
        session_current_after_failure: check.pressured_session.requested_current,
        provisional_release_exact: check.provisional_release_exact,
        external_release_applied: check.external_release_applied,
        failed_peak_unpublished: check.failed_peak_unpublished,
        class_states_exact: check.class_states_exact,
        invariant_error: "internal_invariant",
        no_deadlock: "pass",
        final_process_current,
    }
}

fn concurrent_classes_exact(
    before: &[(StorageAllocationClass, (ImageStorageClassCounters, ImageStorageClassCounters))],
    external: &[(StorageAllocationClass, (ImageStorageClassCounters, ImageStorageClassCounters))],
    final_state: &[(
        StorageAllocationClass,
        (ImageStorageClassCounters, ImageStorageClassCounters),
    )],
) -> bool {
    class_vector_exact(
        before,
        [
            (0, 3, 0, 3),
            (32, 48, 32, 48),
            (0, 480, 0, 480),
            (528, 528, 528, 528),
            (0, 0, 0, 0),
            (4, 12, 4, 12),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ],
    ) && class_vector_exact(
        external,
        [
            (0, 3, 0, 3),
            (0, 48, 64, 128),
            (0, 480, 480, 480),
            (0, 528, 0, 528),
            (0, 0, 48, 48),
            (4, 12, 4, 12),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ],
    ) && class_vector_exact(
        final_state,
        [
            (0, 0, 0, 3),
            (0, 0, 0, 48),
            (0, 0, 0, 480),
            (0, 0, 0, 528),
            (0, 0, 0, 0),
            (0, 0, 4, 12),
            (0, 0, 0, 0),
            (0, 0, 0, 0),
        ],
    )
}

fn class_vector_exact(
    actual: &[(StorageAllocationClass, (ImageStorageClassCounters, ImageStorageClassCounters))],
    expected: [(u64, u64, u64, u64); 8],
) -> bool {
    actual.iter().zip(expected).all(|((_, (session, process)), expected)| {
        let values = (
            session.requested_current,
            session.requested_peak,
            process.requested_current,
            process.requested_peak,
        );
        values == expected
            && session.requested_current == session.observed_current
            && session.requested_peak == session.observed_peak
            && process.requested_current == process.observed_current
            && process.requested_peak == process.observed_peak
    })
}

fn concurrent_release_valid(check: &ConcurrentReleaseCheck<'_>) -> bool {
    matches!(
        check.result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant))
    ) && check.detached_requested == 32
        && check.detached_outputs_requested == 528
        && check.detached_total_requested == 560
        && check.before.requested_current == 564
        && check.before.requested_peak == 1_048
        && check.before.reservation_attempts == 15
        && check.before.allocator_attempts == 12
        && check.before.reserve_before_allocation_calls == 15
        && check.before.observed_reconciliations == 12
        && check.after_external_release.requested_current == 596
        && check.after_external_release.requested_peak == 1_156
        && check.after_external_release.reservation_attempts == 28
        && check.after_external_release.allocator_attempts == 22
        && check.after_external_release.reserve_before_allocation_calls == 28
        && check.after_external_release.observed_reconciliations == 22
        && check.process_after_failure.requested_current == 4
        && check.process_after_failure.requested_peak == 1_048
        && check.process_after_failure.reservation_attempts == 28
        && check.process_after_failure.allocator_attempts == 22
        && check.process_after_failure.reserve_before_allocation_calls == 28
        && check.process_after_failure.observed_reconciliations == 22
        && check.pressured_session.requested_current == 0
        && check.pressured_session.requested_peak == 0
        && check.pressured_session.reservation_attempts == 13
        && check.pressured_session.allocator_attempts == 10
        && check.pressured_session.reserve_before_allocation_calls == 13
        && check.pressured_session.observed_reconciliations == 10
        && check.external_release_applied == "pass"
        && check.provisional_release_exact == "pass"
        && check.failed_peak_unpublished == "pass"
        && check.class_states_exact == "pass"
        && check.foreign_unchanged == "pass"
        && check.failed_sequence == 0
}

async fn verify_ingress_faults() -> Result<IngressFaultEvidence, String> {
    let candidate = verify_candidate_fault().await?;
    let active = verify_active_fault().await?;
    let canonical = verify_canonical_fault().await?;
    let observations = [&candidate, &active, &canonical];
    let client_delivery_once = observations
        .iter()
        .all(|case| case.client.calls == 1 && case.client.bytes == case.bytes.len() as u64);
    let term_feed_once = observations
        .iter()
        .all(|case| case.term.calls == 1 && case.term.bytes == case.bytes.len() as u64);
    let matching_digest = observations.iter().all(|case| {
        case.client.digest == digest(&case.bytes) && case.term.digest == digest(&case.bytes)
    });
    let rejection_callback_once = observations
        .iter()
        .all(|case| case.client.rejection_calls == 1 && case.client.rejection.is_some());
    if !client_delivery_once || !term_feed_once || !matching_digest || !rejection_callback_once {
        return Err("storage error ordinary routing drifted".to_owned());
    }
    Ok(IngressFaultEvidence {
        candidate_error: "counter_overflow",
        active_error: "allocation_failed",
        canonical_error: "session_limit",
        routing: routing_evidence(
            DeliveryEvidence { client_delivery_once, term_feed_once },
            matching_digest,
            candidate.client,
        ),
    })
}

async fn verify_candidate_fault() -> Result<FaultObservation, String> {
    let candidate_policy = TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        64,
        128,
        StorageAllocationClass::FramingCandidate,
        1,
        GraphicsStorageRejection::CounterOverflow,
    );
    let mut candidate = PtyTerminalImageState::new(candidate_policy);
    let candidate_bytes = b"A\x1b".to_vec();
    let (candidate_result, candidate_client, candidate_term) =
        route_observed(&mut candidate, candidate_bytes.clone()).await;
    if !matches!(
        &candidate_result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::CounterOverflow))
    ) || candidate.state().sequence.0 != 0
        || counters(&candidate)?.0.requested_current != 0
    {
        return Err(format!("candidate storage error was hidden: {candidate_result:?}"));
    }
    Ok(FaultObservation { client: candidate_client, term: candidate_term, bytes: candidate_bytes })
}

async fn verify_active_fault() -> Result<FaultObservation, String> {
    let active_policy = TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        64,
        128,
        StorageAllocationClass::FramingActive,
        2,
        GraphicsStorageRejection::AllocationFailed,
    );
    let mut active = PtyTerminalImageState::new(active_policy);
    let active_bytes = b"A\x1b_Ga=q;AAAA\x1b\\".to_vec();
    let (active_result, active_client, active_term) =
        route_observed(&mut active, active_bytes.clone()).await;
    if !matches!(
        &active_result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed))
    ) || active.state().sequence.0 != 0
        || active.state().pending_transfer.is_some()
        || counters(&active)?.0.requested_current != 0
    {
        return Err(format!("active storage error was hidden: {active_result:?}"));
    }
    Ok(FaultObservation { client: active_client, term: active_term, bytes: active_bytes })
}

async fn verify_canonical_fault() -> Result<FaultObservation, String> {
    let canonical_policy = TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        u64::MAX,
        u64::MAX,
        StorageAllocationClass::CanonicalSixel,
        1,
        GraphicsStorageRejection::SessionLimit,
    );
    let mut canonical = PtyTerminalImageState::new(canonical_policy);
    let canonical_bytes = b"A\x1bPq????\x1b\\".to_vec();
    let (canonical_result, canonical_client, canonical_term) =
        route_observed(&mut canonical, canonical_bytes.clone()).await;
    if !matches!(
        &canonical_result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) || canonical.state().sequence.0 != 0
        || canonical.storage_ownership().sixel_body_requested != 0
        || counters(&canonical)?.0.requested_current != 0
        || canonical.validation_rejection_observation() != (1, 1, 0)
    {
        return Err(format!(
            "canonical storage error measurement: result={canonical_result:?} \
             session_limit=max process_limit=max validation_target=CanonicalSixel#1 counters={:?} \
             classes={:?} owner={:?} digests={:?} state={:?} reconcile={:?} target={:?} \
             client={canonical_client:?} term={canonical_term:?}",
            canonical.validation_storage_counters(),
            all_storage_classes(&canonical)?,
            canonical.storage_ownership(),
            canonical.validation_storage_digests(),
            canonical.state(),
            canonical.validation_reconcile_rejection(),
            canonical.validation_rejection_observation(),
        ));
    }
    Ok(FaultObservation { client: canonical_client, term: canonical_term, bytes: canonical_bytes })
}

async fn verify_multi_event_rollback() -> Result<MultiEventRollbackEvidence, String> {
    let mut session = PtyTerminalImageState::new(multi_event_rollback_policy());
    let prior_state = session.state();
    let prior_owner = session.storage_ownership();
    let prior_digests = session.validation_storage_digests();
    let prior_counters = counters(&session)?;
    let bytes = b"A\x1bPq~~~~\x1b\\\x1bPq@@@@\x1b\\".to_vec();
    let (result, client, term) = route_observed(&mut session, bytes.clone()).await;
    let after_state = session.state();
    let after_owner = session.storage_ownership();
    let after_digests = session.validation_storage_digests();
    let after_counters = counters(&session)?;
    let rejection_target = session.validation_rejection_observation();
    let (matching_allocation_attempts, targeted_rejection_fired, staged_before_failure) =
        rejection_target;
    let typed = matches!(
        &result,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::AllocationFailed))
    );
    let state_unchanged = prior_state == after_state;
    let ownership_unchanged = prior_owner == after_owner;
    let digests_unchanged = prior_digests == after_digests;
    let canonical_unchanged = state_unchanged && ownership_unchanged && digests_unchanged;
    let staged_release_exact = ownership_counters_equal(prior_counters, after_counters);
    let target_exact = targeted_rejection_exact(rejection_target);
    let client_delivery_once = client.calls == 1 && client.bytes == bytes.len() as u64;
    let term_feed_once = term.calls == 1 && term.bytes == bytes.len() as u64;
    let matching_digest = client.digest == digest(&bytes) && term.digest == digest(&bytes);
    let expected_telemetry = multi_event_rollback_counters();
    let telemetry_exact =
        after_counters.0 == expected_telemetry && after_counters.1 == expected_telemetry;
    let classes_zero = all_classes_released(&session)?;
    if !typed
        || !canonical_unchanged
        || !staged_release_exact
        || !target_exact
        || !telemetry_exact
        || !classes_zero
        || !client_delivery_once
        || !term_feed_once
        || !matching_digest
        || client.rejection_calls != 1
        || client.rejection.is_none()
    {
        return Err(format!(
            "multi-event rollback drifted: {result:?} {prior_state:?}->{after_state:?} \
             {prior_owner:?}->{after_owner:?} {prior_digests:?}->{after_digests:?} \
             {prior_counters:?}->{after_counters:?}"
        ));
    }
    Ok(MultiEventRollbackEvidence {
        error: "allocation_failed",
        prior_sequence: prior_state.sequence.0,
        sequence_after: after_state.sequence.0,
        prior_sixel_digest: prior_digests.sixel_body,
        sixel_digest_after: after_digests.sixel_body,
        state_before: canonical_state_evidence(prior_state),
        state_after: canonical_state_evidence(after_state),
        accounting_before: current_peak_evidence(prior_counters),
        accounting_after: current_peak_evidence(after_counters),
        state_rollback: StateRollbackEvidence { state_unchanged, ownership_unchanged },
        storage_rollback: StorageRollbackEvidence { digests_unchanged, staged_release_exact },
        canonical_unchanged,
        allocation_class: "canonical_sixel",
        matching_allocation_attempts,
        staged_before_failure,
        targeted_rejection_fired,
        routing: routing_evidence(
            DeliveryEvidence { client_delivery_once, term_feed_once },
            matching_digest,
            client,
        ),
    })
}

fn multi_event_rollback_counters() -> ImageStorageCounters {
    ImageStorageCounters {
        requested_current: 0,
        requested_peak: 0,
        observed_current: 0,
        observed_peak: 0,
        reservation_attempts: 19,
        allocator_attempts: 15,
        reserve_before_allocation_calls: 19,
        observed_reconciliations: 15,
    }
}

fn multi_event_rollback_policy() -> Arc<TerminalImageProcessPolicy> {
    TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        u64::MAX,
        u64::MAX,
        StorageAllocationClass::CanonicalSixel,
        2,
        GraphicsStorageRejection::AllocationFailed,
    )
}

fn targeted_rejection_exact((matching, fired, staged): (u64, u64, u64)) -> bool {
    matching == 2 && staged == 1 && fired == 1
}

fn all_classes_released(session: &PtyTerminalImageState) -> Result<bool, String> {
    Ok(all_storage_classes(session)?.iter().all(|(_, (session, process))| {
        *session == ImageStorageClassCounters::default()
            && *process == ImageStorageClassCounters::default()
    }))
}

/// Consuming a committed metadata vector must not release its paired-ledger
/// ownership while the backing allocation is still alive.
// @lat: [[test#Test Harness#Terminal Image Storage Accounting#Consumed Metadata Ownership]]
fn verify_event_ownership() -> Result<EventOwnershipEvidence, String> {
    let mut session = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let commit = session
        .process_bytes(b"AB\x1bPq~~~~\x1b\\CD")
        .map_err(|error| format!("event ownership commit: {error}"))?;
    let outputs_requested = u64::try_from(commit.outputs.requested_bytes())
        .map_err(|error| format!("output ownership width: {error}"))?;
    if outputs_requested == 0 {
        return Err("committed outputs retained no storage".to_owned());
    }
    let before = session
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?;
    let mut iterator = commit.outputs.into_iter();
    let charged_while_iterating = session
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?
        == before;
    let first = iterator.next();
    let charged_after_partial_drain = session
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?
        == before;
    drop(first);
    drop(iterator);
    let after = session
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?;
    let released_after_iterator_drop = after.0
        == ImageStorageClassCounters {
            requested_peak: before.0.requested_peak,
            observed_peak: before.0.observed_peak,
            ..ImageStorageClassCounters::default()
        }
        && after.1
            == ImageStorageClassCounters {
                requested_peak: before.1.requested_peak,
                observed_peak: before.1.observed_peak,
                ..ImageStorageClassCounters::default()
            };
    if !charged_while_iterating || !charged_after_partial_drain || !released_after_iterator_drop {
        return Err(format!(
            "consumed metadata ownership drifted: while_iterating={charged_while_iterating} \
             partial={charged_after_partial_drain} released={released_after_iterator_drop} \
             before={before:?} after={after:?}"
        ));
    }
    Ok(EventOwnershipEvidence {
        outputs_requested,
        charged_while_iterating: charged_while_iterating.into(),
        charged_after_partial_drain: charged_after_partial_drain.into(),
        released_after_iterator_drop: released_after_iterator_drop.into(),
    })
}

/// Grid observations and their effect vectors must be reserved from the paired
/// ledger before allocation and released exactly once with their commit.
// @lat: [[test#Test Harness#Terminal Image Storage Accounting#Grid Observation Accounting]]
async fn verify_grid_observation_accounting() -> Result<GridObservationEvidence, String> {
    let mut session = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    // Erase-to-end-of-line keeps the shared ingress cell/cursor invariants
    // intact while still producing accounted grid effects on both spans.
    let bytes = b"A\x1b[K\x1bPq~~~~\x1b\\\x1b[K".to_vec();
    let (result, _, _) = route_observed(&mut session, bytes).await;
    let commit = result.map_err(|error| format!("grid observation commit: {error}"))?;
    let effect_count =
        commit.grid_observations().iter().map(|span| span.effects().len()).sum::<usize>();
    if effect_count == 0 || commit.grid_observations().is_empty() {
        return Err("observed span produced no accounted effects".to_owned());
    }
    let held = session
        .validation_storage_class_counters(StorageAllocationClass::GridObservations)
        .map_err(|error| error.to_string())?;
    let accounted = held.0.requested_current > 0
        && held.0 == held.1
        && held.0.observed_current >= held.0.requested_current;
    drop(commit);
    let released = session
        .validation_storage_class_counters(StorageAllocationClass::GridObservations)
        .map_err(|error| error.to_string())?;
    let released_exact = released.0.requested_current == 0
        && released.0.observed_current == 0
        && released.1.requested_current == 0
        && released.1.observed_current == 0;
    let rejected = verify_grid_observation_rejection().await?;
    if !accounted || !released_exact || !rejected {
        return Err(format!(
            "grid observation accounting drifted: accounted={accounted} \
             released={released_exact} rejected={rejected} held={held:?} released={released:?}"
        ));
    }
    Ok(GridObservationEvidence {
        effect_count,
        class_current_while_held: held.0.requested_current,
        class_peak_while_held: held.0.requested_peak,
        accounted_before_allocation: accounted.into(),
        released_after_commit_drop: released_exact.into(),
        rejection: "session_limit",
        rejected_ledger_zero: rejected.into(),
    })
}

async fn verify_grid_observation_rejection() -> Result<bool, String> {
    let policy = TerminalImageProcessPolicy::with_storage_rejection_for_validation(
        u64::MAX,
        u64::MAX,
        StorageAllocationClass::GridObservations,
        1,
        GraphicsStorageRejection::SessionLimit,
    );
    let mut session = PtyTerminalImageState::new(policy);
    let (result, _, _) = route_observed(&mut session, b"A\x1b[K".to_vec()).await;
    let commit = result.map_err(|error| format!("grid rejection commit: {error}"))?;
    let typed = commit.grid_observation_rejection == Some(GraphicsStorageRejection::SessionLimit);
    drop(commit);
    let counters = session
        .validation_storage_class_counters(StorageAllocationClass::GridObservations)
        .map_err(|error| error.to_string())?;
    Ok(typed
        && counters.0.requested_current == 0
        && counters.0.observed_current == 0
        && counters.1.requested_current == 0
        && counters.1.observed_current == 0)
}

/// Work-budget admission must gate decoder initialization: a decode refused by
/// the work ceiling never reserves the buffer whose initialization it refused.
///
/// The payload paints one 100x6 pixel canvas, so initializing it costs 3000
/// work units against a 1000-unit ceiling that parsing alone never reaches.
/// Charging after the fill would leave the 2400-byte canvas reservation in the
/// decoded-Sixel class peak.
// @lat: [[test#Test Harness#Terminal Image Storage Accounting#Work Admission Ordering]]
fn verify_work_admission() -> Result<WorkAdmissionEvidence, String> {
    const CEILING: u64 = 1_000;
    const INITIALIZATION_WORK: u64 = 100 * 6 * 5;
    let mut session = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_work_ceiling_for_validation(CEILING),
    );
    let commit = session
        .process_bytes(b"\x1bPq\"1;1;100;6!100~\x1b\\")
        .map_err(|error| format!("work admission commit: {error}"))?;
    let refused = commit.outputs.iter().any(|output| {
        matches!(
            output,
            SessionTerminalOutput::Image {
                boundary: TerminalImageBoundary::Failure(failure),
                ..
            } if failure.category == GraphicsFailureCategory::QuotaExceeded
        )
    });
    drop(commit);
    let decoded = session
        .validation_storage_class_counters(StorageAllocationClass::DecodedSixel)
        .map_err(|error| error.to_string())?;
    let no_storage = decoded.0 == ImageStorageClassCounters::default()
        && decoded.1 == ImageStorageClassCounters::default();
    let released = counters(&session)?.0.requested_current == 0
        && session.storage_ownership().sixel_body_requested == 0;
    if !refused || !no_storage || !released {
        return Err(format!(
            "work admission drifted: refused={refused} no_storage={no_storage} \
             released={released} decoded={decoded:?}"
        ));
    }
    Ok(WorkAdmissionEvidence {
        admitted_work_units: CEILING,
        refused_initialization_work: INITIALIZATION_WORK,
        sixel_rejection: "quota_exceeded",
        sixel_decoded_peak: decoded.0.requested_peak,
        no_storage_before_admission: no_storage.into(),
        released_after_rejection: released.into(),
    })
}

fn verify_production_formats() -> Result<Vec<FormatEvidence>, String> {
    const PNG: &str = PNG_FIXTURE;
    let rgba = [255, 0, 0, 128];
    let raw = kitty_rgba(1, 1, &rgba);

    let mut zlib_encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib_encoder.write_all(&rgba).map_err(|error| format!("encode zlib Kitty fixture: {error}"))?;
    let zlib_payload =
        zlib_encoder.finish().map_err(|error| format!("finish zlib Kitty fixture: {error}"))?;
    let zlib =
        format!("\x1b_Gf=32,o=z,s=1,v=1;{}\x1b\\", STANDARD.encode(zlib_payload)).into_bytes();

    let png = format!("\x1b_Gf=100;{PNG}\x1b\\").into_bytes();
    let sixel = b"\x1bPq~\x1b\\".to_vec();

    [
        ("raw_rgba", raw, false),
        ("zlib_rgba", zlib, false),
        ("png", png, false),
        ("sixel", sixel, true),
    ]
    .into_iter()
    .map(|(id, bytes, is_sixel)| verify_production_format(id, &bytes, is_sixel))
    .collect()
}

struct KittySplitCase {
    encoded_bytes: usize,
    aggregate: bool,
    transmit_display: bool,
    first_controls: [bool; 3],
    /// First-command controls and their presence republished on the final
    /// boundary of the split transfer.
    final_controls: [bool; 2],
    pending_after_final: usize,
    released_current: u64,
}

struct KittyQueryCase {
    canonical_retained: usize,
    pending_retained: usize,
    ordered: bool,
    publication_count: usize,
    released_current: u64,
}

fn verify_kitty_chunk_protocol() -> Result<KittyChunkEvidence, String> {
    let split = verify_kitty_split_case()?;
    let [first_action, first_ids, first_quiet] = split.first_controls;
    verify_oversized_kitty_chunk()?;
    let count_released = verify_kitty_chunk_count()?;
    let query = verify_kitty_query_case()?;
    let (equal_repeats, conflicting_controls) = verify_kitty_control_repeats()?;
    let valid = split.aggregate
        && split.first_controls.into_iter().all(|check| check)
        && split.final_controls.into_iter().all(|check| check)
        && split.transmit_display
        && split.pending_after_final == 0
        && split.released_current == 0
        && count_released == 0
        && query.canonical_retained == 0
        && query.pending_retained == 0
        && query.ordered
        && query.publication_count == 0
        && query.released_current == 0
        && equal_repeats
        && conflicting_controls;
    if !valid {
        return Err(format!(
            "Kitty chunk protocol drifted: aggregate={} first={}/{}/{} display={} pending={} \
             released={}/{} query={}/{}/{}/{} controls={}/{}",
            split.aggregate,
            first_action,
            first_ids,
            first_quiet,
            split.transmit_display,
            split.pending_after_final,
            split.released_current,
            count_released,
            query.canonical_retained,
            query.pending_retained,
            query.ordered,
            query.publication_count,
            equal_repeats,
            conflicting_controls,
        ));
    }
    Ok(KittyChunkEvidence {
        aggregate_encoded_bytes: split.encoded_bytes,
        aggregate_split_success: split.aggregate.into(),
        individual_chunk_rejection: "kitty_chunk_payload_bytes",
        chunk_count_rejection: "chunks_per_transfer",
        first_action_preserved: first_action.into(),
        first_ids_preserved: first_ids.into(),
        first_quiet_preserved: first_quiet.into(),
        final_controls_preserved: split.final_controls[0].into(),
        final_presence_preserved: split.final_controls[1].into(),
        query_canonical_retained: query.canonical_retained,
        transmit_display_success: split.transmit_display.into(),
        pending_after_final: split.pending_after_final,
        equal_repeats_accepted: equal_repeats.into(),
        conflicting_controls_rejected: conflicting_controls.into(),
        query_boundary_ordered: query.ordered.into(),
        query_publication_count: query.publication_count,
        current_after_release: split
            .released_current
            .saturating_add(query.released_current)
            .saturating_add(count_released),
    })
}

fn verify_kitty_split_case() -> Result<KittySplitCase, String> {
    let rgba = vec![0x7f; 4_096];
    let encoded = STANDARD.encode(&rgba);
    let (first_payload, final_payload) = encoded.split_at(4_096);
    let first_bytes = format!("\x1b_Ga=T,f=32,s=1024,v=1,i=77,p=9,q=2,m=1;{first_payload}\x1b\\");
    let final_bytes = format!("\x1b_Gm=0;{final_payload}\x1b\\");
    let mut split = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let first = split
        .process_bytes(first_bytes.as_bytes())
        .map_err(|error| format!("large first Kitty chunk: {error}"))?;
    let (first_action_preserved, first_ids_preserved, first_quiet_preserved) =
        match first.outputs.as_slice() {
            [
                SessionTerminalOutput::Image {
                    boundary: TerminalImageBoundary::Kitty { command, .. },
                    ..
                },
            ] => (
                command.action == scribe_pty::graphics_framing::KittyAction::TransmitDisplay,
                command.image_id == Some(77) && command.placement_id == Some(9),
                command.quiet == 2,
            ),
            _ => (false, false, false),
        };
    drop(first);
    let final_commit = split
        .process_bytes(final_bytes.as_bytes())
        .map_err(|error| format!("large final Kitty chunk: {error}"))?;
    let transmit_display_success = matches!(
        final_commit.outputs.as_slice(),
        [SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Kitty { .. }, .. }]
    );
    // The final chunk carries only `m=0`; its published boundary must still
    // repeat the transfer's first-command controls and their presence.
    let (final_controls_preserved, final_presence_preserved) = match final_commit.outputs.as_slice()
    {
        [
            SessionTerminalOutput::Image {
                boundary: TerminalImageBoundary::Kitty { command, .. },
                ..
            },
        ] => (
            command.action == scribe_pty::graphics_framing::KittyAction::TransmitDisplay
                && command.format == Some(scribe_pty::graphics_framing::KittyFormat::Rgba)
                && command.image_id == Some(77)
                && command.placement_id == Some(9)
                && command.width == Some(1024)
                && command.height == Some(1)
                && command.quiet == 2,
            command.control_present(b'a')
                && command.control_present(b'f')
                && command.control_present(b'i')
                && command.control_present(b'p')
                && command.control_present(b'q')
                && !command.control_present(b'z'),
        ),
        _ => (false, false),
    };
    drop(final_commit);
    let split_owner = split.storage_ownership();
    let aggregate_split_success = split_owner.kitty_decoded_requested == rgba.len()
        && split.validation_storage_digests().kitty_decoded == digest(&rgba);
    split.release_retained_storage();
    let split_released = counters(&split)?.0;

    Ok(KittySplitCase {
        encoded_bytes: encoded.len(),
        aggregate: aggregate_split_success,
        first_controls: [first_action_preserved, first_ids_preserved, first_quiet_preserved],
        final_controls: [final_controls_preserved, final_presence_preserved],
        transmit_display: transmit_display_success,
        pending_after_final: split_owner.pending_kitty_requested,
        released_current: split_released.requested_current,
    })
}

fn verify_oversized_kitty_chunk() -> Result<(), String> {
    let oversized_payload = "A".repeat(4_100);
    let oversized_bytes = format!("\x1b_Gf=32,s=1,v=1;{oversized_payload}\x1b\\");
    let mut oversized = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let oversized_commit = oversized
        .process_bytes(oversized_bytes.as_bytes())
        .map_err(|error| format!("oversized Kitty framing: {error}"))?;
    let individual_chunk_rejected = matches!(
        oversized_commit.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            boundary: TerminalImageBoundary::Failure(failure),
            ..
        }] if failure.category == scribe_pty::graphics_framing::GraphicsFailureCategory::QuotaExceeded
    );
    if !individual_chunk_rejected {
        return Err(format!("oversized Kitty chunk was admitted: {:?}", oversized_commit.outputs));
    }
    Ok(())
}

fn verify_kitty_chunk_count() -> Result<u64, String> {
    let mut chunk_count = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    advance_kitty_chunk_count(&mut chunk_count)?;
    let count_before_state = chunk_count.validation_pending_kitty_decode_state();
    let count_before_owner = chunk_count.storage_ownership();
    let count_before_counters = counters(&chunk_count)?;
    let count_before_digest = chunk_count.validation_storage_digests();
    let count_rejection = chunk_count
        .process_bytes(b"\x1b_Gm=0;AAAA\x1b\\")
        .map_err(|error| format!("chunk-count rejecting continuation: {error}"))?;
    let chunk_count_rejected = matches!(
        count_rejection.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            sequence,
            boundary: TerminalImageBoundary::Failure(failure),
            ..
        }] if sequence.0 == 32_769
            && failure.category
                == scribe_pty::graphics_framing::GraphicsFailureCategory::QuotaExceeded
    );
    let count_output = format!("{:?}", count_rejection.outputs);
    drop(count_rejection);
    let count_after_state = chunk_count.validation_pending_kitty_decode_state();
    let count_after_owner = chunk_count.storage_ownership();
    let count_after_counters = counters(&chunk_count)?;
    let count_after_digest = chunk_count.validation_storage_digests();
    let expected_count_before = ImageStorageCounters {
        requested_current: 98_304,
        requested_peak: 148_472,
        observed_current: 98_304,
        observed_peak: 148_472,
        reservation_attempts: 327_698,
        allocator_attempts: 229_394,
        reserve_before_allocation_calls: 327_698,
        observed_reconciliations: 229_394,
    };
    let expected_count_after = ImageStorageCounters {
        requested_current: 0,
        requested_peak: 148_472,
        observed_current: 0,
        observed_peak: 148_472,
        reservation_attempts: 327_708,
        allocator_attempts: 229_401,
        reserve_before_allocation_calls: 327_708,
        observed_reconciliations: 229_401,
    };
    if count_before_state != Some((32_768, 98_304, false))
        || count_before_owner.pending_kitty_requested != 98_304
        || count_before_owner.pending_kitty_observed != 98_304
        || count_before_digest.pending_kitty != 0
        || count_before_counters.0 != expected_count_before
        || count_before_counters.1 != expected_count_before
        || count_after_state.is_some()
        || count_after_owner.pending_kitty_requested != 0
        || count_after_digest.pending_kitty != 0
        || count_after_counters.0 != expected_count_after
        || count_after_counters.1 != expected_count_after
        || !chunk_count_rejected
    {
        return Err(format!(
            "chunk-count quota rollback drifted: state={count_before_state:?}->{count_after_state:?} \
             owner={count_before_owner:?}->{count_after_owner:?} counters=\
             {count_before_counters:?}->{count_after_counters:?} \
             digest={count_before_digest:?}->{count_after_digest:?} output={count_output}"
        ));
    }
    chunk_count.release_retained_storage();
    let count_released = counters(&chunk_count)?.0;
    Ok(count_released.requested_current.saturating_add(count_released.observed_current))
}

fn advance_kitty_chunk_count(chunk_count: &mut PtyTerminalImageState) -> Result<(), String> {
    let first = chunk_count
        .process_bytes(b"\x1b_Gf=32,s=4096,v=6,m=1;AAAA\x1b\\")
        .map_err(|error| format!("chunk-count first chunk: {error}"))?;
    if !matches!(
        first.outputs.as_slice(),
        [SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Kitty { .. }, .. }]
    ) || chunk_count.validation_pending_kitty_decode_state() != Some((1, 3, false))
    {
        return Err(format!("chunk-count first valid chunk drifted: {:?}", first.outputs));
    }
    drop(first);
    for ordinal in 2..=ImageLimits::V1.max_chunks_per_transfer {
        let commit = chunk_count
            .process_bytes(b"\x1b_Gm=1;AAAA\x1b\\")
            .map_err(|error| format!("chunk-count admitted continuation: {error}"))?;
        let decoded_len = usize::try_from(ordinal).ok().and_then(|value| value.checked_mul(3));
        let state_exact = decoded_len.is_some_and(|len| {
            chunk_count.validation_pending_kitty_decode_state() == Some((ordinal, len, false))
        });
        let output_exact = matches!(
            commit.outputs.as_slice(),
            [SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Kitty { .. }, .. }]
        );
        if !state_exact || !output_exact {
            return Err(format!(
                "chunk-count admitted ordinal {ordinal} drifted: state={:?} outputs={:?}",
                chunk_count.validation_pending_kitty_decode_state(),
                commit.outputs,
            ));
        }
    }
    Ok(())
}

fn verify_kitty_query_case() -> Result<KittyQueryCase, String> {
    let mut query = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let query_first = query
        .process_bytes(b"\x1b_Ga=q,f=32,s=1,v=1,i=91,p=7,q=2,m=1;/wAA\x1b\\")
        .map_err(|error| format!("split query first chunk: {error}"))?;
    let query_first_ordered = matches!(
        query_first.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            sequence,
            boundary: TerminalImageBoundary::Kitty { command, .. },
            ..
        }] if sequence.0 == 1
            && command.action == scribe_pty::graphics_framing::KittyAction::Query
            && command.image_id == Some(91)
            && command.placement_id == Some(7)
            && command.quiet == 2
    );
    drop(query_first);
    let query_final = query
        .process_bytes(b"\x1b_Gm=0;gA==\x1b\\")
        .map_err(|error| format!("split query final chunk: {error}"))?;
    let query_final_ordered = matches!(
        query_final.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            sequence,
            boundary: TerminalImageBoundary::Kitty { .. },
            ..
        }] if sequence.0 == 2
    );
    drop(query_final);
    let query_owner = query.storage_ownership();
    query.release_retained_storage();
    let query_released = counters(&query)?.0;
    let query_state = query.state();

    Ok(KittyQueryCase {
        canonical_retained: query_owner.kitty_decoded_requested,
        pending_retained: query_owner.pending_kitty_requested,
        ordered: query_first_ordered && query_final_ordered,
        publication_count: query_state.definition_count + query_state.placement_count,
        released_current: query_released
            .requested_current
            .saturating_add(query_released.observed_current),
    })
}

fn verify_kitty_control_repeats() -> Result<(bool, bool), String> {
    let mut equal = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    drop(
        equal
            .process_bytes(b"\x1b_Gf=32,s=1,v=1,i=5,q=1,m=1;/wAA\x1b\\")
            .map_err(|error| format!("equal-control first chunk: {error}"))?,
    );
    let equal_final = equal
        .process_bytes(b"\x1b_Gf=32,s=1,v=1,i=5,q=1,m=0;gA==\x1b\\")
        .map_err(|error| format!("equal-control final chunk: {error}"))?;
    let equal_repeats_accepted = matches!(
        equal_final.outputs.as_slice(),
        [SessionTerminalOutput::Image { boundary: TerminalImageBoundary::Kitty { .. }, .. }]
    ) && equal.storage_ownership().kitty_decoded_requested == 4;
    drop(equal_final);
    equal.release_retained_storage();

    let mut conflict = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    drop(
        conflict
            .process_bytes(b"\x1b_Gf=32,s=1,v=1,i=5,q=1,m=1;/wAA\x1b\\")
            .map_err(|error| format!("conflicting-control first chunk: {error}"))?,
    );
    let conflict_final = conflict
        .process_bytes(b"\x1b_Gs=2,m=0;gA==\x1b\\")
        .map_err(|error| format!("conflicting-control final chunk: {error}"))?;
    let conflicting_controls_rejected = matches!(
        conflict_final.outputs.as_slice(),
        [SessionTerminalOutput::Image {
            boundary: TerminalImageBoundary::Failure(failure),
            ..
        }] if failure.category
            == scribe_pty::graphics_framing::GraphicsFailureCategory::MalformedControl
    ) && conflict.storage_ownership()
        == ImageStorageOwnership::default()
        && conflict.state().definition_count == 0
        && conflict.state().placement_count == 0;
    drop(conflict_final);
    conflict.release_retained_storage();
    Ok((equal_repeats_accepted, conflicting_controls_rejected))
}

fn verify_production_format(
    id: &'static str,
    bytes: &[u8],
    is_sixel: bool,
) -> Result<FormatEvidence, String> {
    let mut baseline = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let commit = baseline
        .process_bytes(bytes)
        .map_err(|error| format!("{id} unbounded production ingress: {error}"))?;
    let measured_peak = counters(&baseline)?.0.requested_peak;
    drop(commit);
    let retained = baseline.storage_ownership();
    let digests = baseline.validation_storage_digests();
    let retained_counters = counters(&baseline)?.0;
    let (decoded_requested, decoded_observed, decoded_digest) = if is_sixel {
        (retained.sixel_decoded_requested, retained.sixel_decoded_observed, digests.sixel_decoded)
    } else {
        (retained.kitty_decoded_requested, retained.kitty_decoded_observed, digests.kitty_decoded)
    };
    baseline.release_retained_storage();
    let baseline_released = counters(&baseline)?.0.requested_current;

    let mut exact = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(measured_peak, u64::MAX),
    );
    let exact_commit = exact
        .process_bytes(bytes)
        .map_err(|error| format!("{id} exact {measured_peak} production ingress: {error}"))?;
    drop(exact_commit);
    exact.release_retained_storage();
    let exact_success = counters(&exact)?.0.requested_current == 0;

    let mut rejected =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            measured_peak.saturating_sub(1),
            u64::MAX,
        ));
    let before_state = rejected.state();
    let before_owners = rejected.storage_ownership();
    let before_digests = rejected.validation_storage_digests();
    let before_counters = counters(&rejected)?;
    let rejection = rejected.process_bytes(bytes);
    let rollback_unchanged = matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) && before_state == rejected.state()
        && before_owners == rejected.storage_ownership()
        && before_digests == rejected.validation_storage_digests()
        && ownership_counters_equal(before_counters, counters(&rejected)?);
    rejected.release_retained_storage();
    let rejected_released = counters(&rejected)?.0.requested_current;

    if measured_peak == 0
        || decoded_requested == 0
        || decoded_observed < decoded_requested
        || decoded_digest == 0
        || baseline_released != 0
        || !exact_success
        || !rollback_unchanged
        || rejected_released != 0
    {
        return Err(format!(
            "{id} production format evidence drifted: peak={measured_peak} retained={retained:?} \
             counters={retained_counters:?} rejection={rejection:?}"
        ));
    }
    Ok(FormatEvidence {
        id,
        measured_peak,
        retained_current: retained_counters.requested_current,
        decoded_requested,
        decoded_observed,
        decoded_digest,
        exact_success,
        max_minus_one_rejection: "session_limit",
        rollback_unchanged,
        current_after_release: baseline_released.saturating_add(rejected_released),
    })
}

fn verify_metadata_boundaries() -> Result<MetadataEvidence, String> {
    let mut bytes = Vec::new();
    for _ in 0..64 {
        bytes.extend_from_slice(b"\x1bx");
    }
    let mut baseline = PtyTerminalImageState::new(
        TerminalImageProcessPolicy::with_storage_limits_for_validation(u64::MAX, u64::MAX),
    );
    let commit = baseline
        .process_bytes(&bytes)
        .map_err(|error| format!("metadata baseline ingress: {error}"))?;
    let measured_total_peak = counters(&baseline)?.0.requested_peak;
    let events = baseline
        .validation_storage_class_counters(StorageAllocationClass::FramingEvents)
        .map_err(|error| error.to_string())?
        .0;
    let outputs = baseline
        .validation_storage_class_counters(StorageAllocationClass::TerminalOutputs)
        .map_err(|error| error.to_string())?
        .0;
    drop(commit);
    baseline.release_retained_storage();
    let baseline_released = counters(&baseline)?.0.requested_current;

    let mut exact =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            measured_total_peak,
            u64::MAX,
        ));
    let exact_commit =
        exact.process_bytes(&bytes).map_err(|error| format!("metadata exact ingress: {error}"))?;
    drop(exact_commit);
    exact.release_retained_storage();
    let exact_success = counters(&exact)?.0.requested_current == 0;

    let mut rejected =
        PtyTerminalImageState::new(TerminalImageProcessPolicy::with_storage_limits_for_validation(
            measured_total_peak.saturating_sub(1),
            u64::MAX,
        ));
    let before_state = rejected.state();
    let before_owners = rejected.storage_ownership();
    let before_digests = rejected.validation_storage_digests();
    let before_counters = counters(&rejected)?;
    let rejection = rejected.process_bytes(&bytes);
    let rollback_unchanged = matches!(
        rejection,
        Err(SessionTerminalError::Storage(GraphicsStorageRejection::SessionLimit))
    ) && before_state == rejected.state()
        && before_owners == rejected.storage_ownership()
        && before_digests == rejected.validation_storage_digests()
        && ownership_counters_equal(before_counters, counters(&rejected)?);
    rejected.release_retained_storage();
    let rejected_released = counters(&rejected)?.0.requested_current;
    if events.requested_peak == 0
        || outputs.requested_peak == 0
        || events.observed_peak < events.requested_peak
        || outputs.observed_peak < outputs.requested_peak
        || !exact_success
        || !rollback_unchanged
        || baseline_released != 0
        || rejected_released != 0
    {
        return Err(format!(
            "metadata accounting drifted: events={events:?} outputs={outputs:?} \
             peak={measured_total_peak} rejection={rejection:?}"
        ));
    }
    Ok(MetadataEvidence {
        input_bytes: bytes.len(),
        event_requested_peak: events.requested_peak,
        event_observed_peak: events.observed_peak,
        output_requested_peak: outputs.requested_peak,
        output_observed_peak: outputs.observed_peak,
        measured_total_peak,
        exact_success,
        max_minus_one_rejection: "session_limit",
        rollback_unchanged,
        current_after_release: baseline_released.saturating_add(rejected_released),
    })
}

async fn route(
    session: &mut PtyTerminalImageState,
    bytes: Vec<u8>,
) -> Result<SessionTerminalCommit, String> {
    process_pty_reader_ingress(
        session,
        bytes,
        |_| {},
        |_observer, _bytes, image_result| async move { (image_result, None) },
        |_rejection| {},
    )
    .await
    .map_err(|error| error.to_string())
}

async fn route_observed(
    session: &mut PtyTerminalImageState,
    bytes: Vec<u8>,
) -> (Result<SessionTerminalCommit, SessionTerminalError>, ObservedSink, ObservedSink) {
    let client = Rc::new(RefCell::new(ObservedSink::default()));
    let term = Rc::new(tokio::sync::Mutex::new(RealTermFeed::new()));
    let client_sink = Rc::clone(&client);
    let rejection_sink = Rc::clone(&client);
    let term_sink = Rc::clone(&term);
    let result = process_pty_reader_ingress(
        session,
        bytes,
        move |bytes| client_sink.borrow_mut().observe(bytes),
        move |observer, bytes, mut image_result| async move {
            let mut feed = term_sink.lock().await;
            let RealTermFeed { term: real_term, processor, .. } = &mut *feed;
            let (result, observation) = feed_terminal_image_result_production(
                ProductionTerminalFeed::new(&observer, real_term, processor),
                bytes.as_ref(),
                image_result,
                || {},
            )
            .await;
            image_result = result;
            let Some(observation) = observation else {
                return (
                    Err(SessionTerminalError::Storage(GraphicsStorageRejection::InternalInvariant)),
                    None,
                );
            };
            feed.observed.observe(bytes.as_ref());
            let term_handle = Arc::clone(&feed.term);
            let term_guard = term_handle.lock().await;
            let term_cursor_column = term_guard.grid().cursor.point.column.0;
            let term_cell = term_guard.grid()[Line(0)][Column(0)].c;
            let observer_cursor_column =
                observation.observation.primary.cursor.map_or(0, |cursor| cursor.column);
            let real_term_feed =
                term_cursor_column == 1 && term_cell == 'A' && observer_cursor_column == 1;
            drop(term_guard);
            feed.observed.real_term_feed = real_term_feed;
            feed.observed.term_cursor_column = term_cursor_column;
            feed.observed.term_cell = term_cell;
            feed.observed.observer_cursor_column = observer_cursor_column;
            (image_result, Some(observation))
        },
        move |rejection| rejection_sink.borrow_mut().observe_rejection(rejection),
    )
    .await;
    let client_observed = *client.borrow();
    let term_observed = term.lock().await.observed;
    assert!(term_observed.real_term_feed, "accounting ingress bypassed real Alacritty Term feed");
    (result, client_observed, term_observed)
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |digest, byte| {
        digest.wrapping_mul(1_099_511_628_211).wrapping_add(u64::from(*byte))
    })
}

fn kitty_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    format!("\x1b_Gf=32,s={width},v={height};{}\x1b\\", STANDARD.encode(rgba)).into_bytes()
}

fn delivery_checks(
    bytes: &[u8],
    client: ObservedSink,
    term: ObservedSink,
) -> (DeliveryEvidence, bool) {
    let expected_digest = digest(bytes);
    (
        DeliveryEvidence {
            client_delivery_once: client.calls == 1 && client.bytes == bytes.len() as u64,
            term_feed_once: term.calls == 1
                && term.bytes == bytes.len() as u64
                && term.real_term_feed,
        },
        client.digest == expected_digest && term.digest == expected_digest,
    )
}

fn attempt_deltas(
    protocol: &str,
    before: ImageStorageCounters,
    after: ImageStorageCounters,
) -> Result<AttemptDeltas, String> {
    Ok(AttemptDeltas {
        reservation: after
            .reservation_attempts
            .checked_sub(before.reservation_attempts)
            .ok_or_else(|| format!("{protocol} reservation attempts regressed"))?,
        allocator: after
            .allocator_attempts
            .checked_sub(before.allocator_attempts)
            .ok_or_else(|| format!("{protocol} allocator attempts regressed"))?,
        reserve_call: after
            .reserve_before_allocation_calls
            .checked_sub(before.reserve_before_allocation_calls)
            .ok_or_else(|| format!("{protocol} reserve calls regressed"))?,
        reconcile: after
            .observed_reconciliations
            .checked_sub(before.observed_reconciliations)
            .ok_or_else(|| format!("{protocol} reconciliations regressed"))?,
    })
}

fn routing_evidence(
    delivery: DeliveryEvidence,
    matching_digest: bool,
    rejection: ObservedSink,
) -> RoutingEvidence {
    RoutingEvidence {
        delivery,
        matching_digest,
        rejection: RejectionRoutingEvidence {
            rejection_callback_once: rejection.rejection_calls == 1
                && rejection.rejection.is_some(),
            rejection_payload_free: true,
        },
    }
}

fn verify_framer_retry_faults() -> Result<FramerRetryEvidence, String> {
    let complete = b"\x1b_Gf=32,s=1,v=1;AQIDBA==\x1b\\";
    let (candidate_exact_rollback, candidate_retry_events, candidate_retry_exact, candidate_trace) =
        verify_framer_push_retry(
            StorageAllocationClass::FramingCandidate,
            1,
            GraphicsStorageRejection::CounterOverflow,
            "candidate",
            complete,
        )?;
    let (active_exact_rollback, active_retry_events, active_retry_exact, active_trace) =
        verify_framer_push_retry(
            StorageAllocationClass::FramingActive,
            2,
            GraphicsStorageRejection::AllocationFailed,
            "active",
            complete,
        )?;

    let eof_budget = framer_fault_budget(
        StorageAllocationClass::FramingEvents,
        2,
        GraphicsStorageRejection::AllocationFailed,
    );
    let mut eof = GraphicsFramer::with_storage_budget(
        usize::try_from(ImageLimits::V1.max_control_string_bytes)
            .map_err(|_| "EOF control limit exceeds usize".to_owned())?,
        Arc::clone(&eof_budget),
    );
    let incomplete_events =
        eof.push(b"\x1b_Gf=32,s=1,v=1;AQID").map_err(|error| format!("EOF setup: {error:?}"))?;
    if !incomplete_events.is_empty() {
        return Err(format!("EOF setup published early: {incomplete_events:?}"));
    }
    drop(incomplete_events);
    let eof_before = eof.validation_snapshot();
    let eof_classes_before = eof_budget
        .class_counters(StorageAllocationClass::FramingActive)
        .map_err(|error| format!("EOF counters before: {error:?}"))?;
    let eof_failure = eof.finish();
    let eof_after = eof.validation_snapshot();
    let eof_classes_after = eof_budget
        .class_counters(StorageAllocationClass::FramingActive)
        .map_err(|error| format!("EOF counters after: {error:?}"))?;
    let eof_exact_rollback = matches!(eof_failure, Err(GraphicsStorageRejection::AllocationFailed))
        && eof_before == eof_after
        && eof_classes_before == eof_classes_after
        && eof_budget.validation_rejection_observation() == (2, 1, 0);
    let eof_retry = eof.finish().map_err(|error| format!("EOF retry: {error:?}"))?;
    let eof_retry_events = eof_retry.len();
    let eof_retry_exact = matches!(
        eof_retry.as_slice(),
        [GraphicsEvent::Failure(failure)]
            if failure.category == GraphicsFailureCategory::TruncatedSequence
    );
    drop(eof_retry);

    let no_duplicate_publication =
        candidate_retry_events == 1 && active_retry_events == 1 && eof_retry_events == 1;
    if !candidate_exact_rollback
        || !candidate_retry_exact
        || !active_exact_rollback
        || !active_retry_exact
        || !eof_exact_rollback
        || !eof_retry_exact
        || !no_duplicate_publication
    {
        return Err(format!(
            "retry rollback drifted: candidate={candidate_trace} active={active_trace} \
             eof={eof_before:?}->{eof_after:?} \
             events={candidate_retry_events}/{active_retry_events}/{eof_retry_events}"
        ));
    }
    Ok(FramerRetryEvidence {
        candidate_exact_rollback: candidate_exact_rollback.into(),
        candidate_retry_events,
        active_exact_rollback: active_exact_rollback.into(),
        active_retry_events,
        eof_exact_rollback: eof_exact_rollback.into(),
        eof_retry_events,
        no_duplicate_publication: no_duplicate_publication.into(),
    })
}

fn verify_framer_push_retry(
    class: StorageAllocationClass,
    ordinal: u64,
    rejection: GraphicsStorageRejection,
    label: &str,
    complete: &[u8],
) -> Result<(bool, usize, bool, String), String> {
    let budget = framer_fault_budget(class, ordinal, rejection);
    let limit = usize::try_from(ImageLimits::V1.max_control_string_bytes)
        .map_err(|_| format!("{label} control limit exceeds usize"))?;
    let mut framer = GraphicsFramer::with_storage_budget(limit, Arc::clone(&budget));
    let before = framer.validation_snapshot();
    let classes_before = budget
        .class_counters(class)
        .map_err(|error| format!("{label} counters before: {error:?}"))?;
    let failure = framer.push(complete);
    let after = framer.validation_snapshot();
    let classes_after = budget
        .class_counters(class)
        .map_err(|error| format!("{label} counters after: {error:?}"))?;
    let exact_rollback = matches!(failure, Err(actual) if actual == rejection)
        && before == after
        && classes_before == classes_after
        && budget.validation_rejection_observation() == (ordinal, 1, 0);
    let retry = framer.push(complete).map_err(|error| format!("{label} retry: {error:?}"))?;
    let retry_events = retry.len();
    let retry_exact = matches!(retry.as_slice(), [GraphicsEvent::Kitty { .. }]);
    Ok((exact_rollback, retry_events, retry_exact, format!("{before:?}->{after:?}")))
}

fn framer_fault_budget(
    class: StorageAllocationClass,
    matching_ordinal: u64,
    rejection: GraphicsStorageRejection,
) -> Arc<DecodeStorage> {
    DecodeStorage::new(
        StorageProcess::new(u64::MAX),
        u64::MAX,
        0,
        StorageValidation {
            rejection: Some(StorageValidationRejection { class, matching_ordinal, rejection }),
            ..StorageValidation::default()
        },
    )
}

fn current_peak_evidence(
    counters: (ImageStorageCounters, ImageStorageCounters),
) -> CurrentPeakEvidence {
    CurrentPeakEvidence {
        session_current: counters.0.requested_current,
        session_peak: counters.0.requested_peak,
        process_current: counters.1.requested_current,
        process_peak: counters.1.requested_peak,
    }
}

fn sixel_class_decomposition(
    session: &PtyTerminalImageState,
) -> Result<Vec<NamedStorageClassPair>, String> {
    [
        ("candidate", StorageAllocationClass::FramingCandidate),
        ("active", StorageAllocationClass::FramingActive),
        ("events", StorageAllocationClass::FramingEvents),
        ("outputs", StorageAllocationClass::TerminalOutputs),
        ("canonical_sixel", StorageAllocationClass::CanonicalSixel),
        ("decoded_sixel", StorageAllocationClass::DecodedSixel),
    ]
    .into_iter()
    .map(|(name, class)| {
        session
            .validation_storage_class_counters(class)
            .map(|pair| (name, pair))
            .map_err(|error| error.to_string())
    })
    .collect()
}

fn all_storage_classes(
    session: &PtyTerminalImageState,
) -> Result<Vec<AllocationStorageClassPair>, String> {
    [
        StorageAllocationClass::FramingCandidate,
        StorageAllocationClass::FramingActive,
        StorageAllocationClass::FramingEvents,
        StorageAllocationClass::TerminalOutputs,
        StorageAllocationClass::CanonicalSixel,
        StorageAllocationClass::DecodedKitty,
        StorageAllocationClass::DecodedSixel,
        StorageAllocationClass::GridObservations,
    ]
    .into_iter()
    .map(|class| {
        session
            .validation_storage_class_counters(class)
            .map(|pair| (class, pair))
            .map_err(|error| error.to_string())
    })
    .collect()
}

fn storage_class_exact(counters: ImageStorageClassCounters, current: u64, peak: u64) -> bool {
    counters.requested_current == current
        && counters.observed_current == current
        && counters.requested_peak == peak
        && counters.observed_peak == peak
}

fn storage_telemetry(counters: ImageStorageCounters) -> StorageTelemetryEvidence {
    StorageTelemetryEvidence {
        requested_current: counters.requested_current,
        requested_peak: counters.requested_peak,
        observed_current: counters.observed_current,
        observed_peak: counters.observed_peak,
        reservation_attempts: counters.reservation_attempts,
        allocator_attempts: counters.allocator_attempts,
        reserve_before_allocation_calls: counters.reserve_before_allocation_calls,
        observed_reconciliations: counters.observed_reconciliations,
    }
}

fn canonical_state_evidence(state: SessionTerminalState) -> CanonicalStateEvidence {
    CanonicalStateEvidence {
        generation: state.generation.0,
        sequence: state.sequence.0,
        active_screen: match state.active_screen {
            TerminalScreenKind::Primary => "primary",
            TerminalScreenKind::Alternate => "alternate",
        },
        definition_count: state.definition_count,
        placement_count: state.placement_count,
        pending_transfer: state.pending_transfer.is_some(),
    }
}

fn ownership_counters_equal(
    before: (ImageStorageCounters, ImageStorageCounters),
    after: (ImageStorageCounters, ImageStorageCounters),
) -> bool {
    [(before.0, after.0), (before.1, after.1)].into_iter().all(|(before, after)| {
        before.requested_current == after.requested_current
            && before.requested_peak == after.requested_peak
            && before.observed_current == after.observed_current
            && before.observed_peak == after.observed_peak
    })
}

fn counters(
    session: &PtyTerminalImageState,
) -> Result<(ImageStorageCounters, ImageStorageCounters), String> {
    session.storage_counters().map_err(|error| error.to_string())
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode accounting evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
