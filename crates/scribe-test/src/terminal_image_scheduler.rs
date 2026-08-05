//! Production-path evidence for mandatory terminal-image decode scheduling.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use scribe_image_decode::{
    DecodeAdmissionError, DecodeBudget, DecodeCeilings, DecodeLimits, DecodePermit, DecodeRequest,
    DecodeScheduler, DecodeSchedulerMetrics, DecodeSessionId, DecodeStorage, DecodeTarget,
    NoopHooks,
};
use scribe_server::terminal_image_state::{
    PtyTerminalImageState, SessionTerminalOutput, TerminalImageBoundary, TerminalImageProcessPolicy,
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

/// One complete single-chunk Sixel command.
const SIXEL_FIXTURE: &[u8] = b"\x1bPq~\x1b\\";

#[derive(Serialize)]
struct Evidence {
    schema_version: u32,
    status: &'static str,
    engine: &'static str,
    admission: &'static str,
    production: ProductionEvidence,
    capabilities: CapabilityEvidence,
    fifo: FifoEvidence,
    cancellation: CancellationEvidence,
    deadline: DeadlineEvidence,
    bounded: BoundedEvidence,
    independent_session: IndependentSessionEvidence,
    final_ownership: OwnershipEvidence,
}

/// Every production decode entry point passed through the scheduler.
#[derive(Serialize)]
struct ProductionEvidence {
    kitty_images: usize,
    sixel_images: usize,
    production_admitted: u64,
    production_released: u64,
    released_exactly_once: EvidenceCheck,
    queued_after: u32,
    active_after: u32,
}

/// Foreign capabilities are refused before any decode work is charged.
#[derive(Serialize)]
struct CapabilityEvidence {
    foreign_issuer: &'static str,
    foreign_ticket_issuer: &'static str,
    foreign_session: &'static str,
    foreign_generation: &'static str,
    foreign_target: &'static str,
    foreign_budget: &'static str,
    foreign_budget_bytes: &'static str,
    request_exceeds_ceiling: &'static str,
    rejected_before_work: EvidenceCheck,
}

/// Admission follows issue order and a later caller cannot barge.
#[derive(Serialize)]
struct FifoEvidence {
    issue_order: Vec<u64>,
    admission_order: Vec<u64>,
    barged: EvidenceCheck,
}

/// Cancellation retires exactly its own target and wakes the successor.
#[derive(Serialize)]
struct CancellationEvidence {
    cancelled_waiter: &'static str,
    successor_admitted: EvidenceCheck,
    successor_not_cancelled: EvidenceCheck,
    in_flight_cancelled: EvidenceCheck,
    in_flight_decode_refused: EvidenceCheck,
    unrelated_target_untouched: EvidenceCheck,
    released_exactly_once: EvidenceCheck,
}

/// A queue-wait deadline retires the waiter and leaves the queue drainable.
#[derive(Serialize)]
struct DeadlineEvidence {
    expired_waiter: &'static str,
    expired_total: u64,
    successor_admitted_after_release: EvidenceCheck,
    queued_after: u32,
}

/// Queue metadata stays inside the immutable ceilings.
#[derive(Serialize)]
struct BoundedEvidence {
    queue_depth_ceiling: u32,
    queue_full: &'static str,
    peak_queued: u32,
    abandoned_pruned: u64,
    queued_after_abandon: u32,
}

/// An unrelated session decodes while another session holds a slot.
#[derive(Serialize)]
struct IndependentSessionEvidence {
    holder_session: u64,
    progressing_session: u64,
    progressed: EvidenceCheck,
    images: usize,
}

/// Nothing is queued, active, or charged once every probe finished.
#[derive(Serialize)]
struct OwnershipEvidence {
    queued: u32,
    active: u32,
    admitted: u64,
    released: u64,
    session_requested_current: u64,
    process_requested_current: u64,
}

/// Build one complete single-chunk Kitty RGBA transmit command.
fn kitty_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    format!("\x1b_Gf=32,s={width},v={height};{}\x1b\\", STANDARD.encode(rgba)).into_bytes()
}

fn ceilings(concurrent: u32, depth: u32, wait: Duration) -> DecodeCeilings {
    DecodeCeilings {
        concurrent_decodes: concurrent,
        queue_depth: depth,
        queue_bytes: 1 << 20,
        queue_wait: wait,
    }
}

fn request(
    session: DecodeSessionId,
    target: DecodeTarget,
    storage: &Arc<DecodeStorage>,
) -> DecodeRequest {
    DecodeRequest {
        session,
        generation: 1,
        target,
        requested_bytes: 4,
        storage: Arc::clone(storage),
    }
}

/// Issue and admit one request on the calling thread.
fn hold(
    scheduler: &Arc<DecodeScheduler>,
    request: DecodeRequest,
) -> Result<DecodePermit, DecodeAdmissionError> {
    let ticket = scheduler.issue(request)?;
    scheduler.admit(ticket)
}

fn metrics(scheduler: &Arc<DecodeScheduler>) -> Result<DecodeSchedulerMetrics, String> {
    scheduler.metrics().map_err(|error| format!("scheduler metrics: {error}"))
}

fn count_images(state: &mut PtyTerminalImageState, bytes: &[u8]) -> Result<usize, String> {
    let commit = state.process_bytes(bytes).map_err(|error| error.to_string())?;
    Ok(commit
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
        .count())
}

/// Drive real Kitty and Sixel bytes through the production seam and prove both
/// decode entry points took, and returned, one scheduler admission.
// @lat: [[test#Test Harness#Mandatory Decode Scheduling#Production Admission Probe]]
fn verify_production() -> Result<(ProductionEvidence, OwnershipEvidence), String> {
    let policy = TerminalImageProcessPolicy::with_decode_ceilings_for_validation(ceilings(
        2,
        8,
        Duration::from_secs(5),
    ));
    let scheduler = Arc::clone(policy.decode_scheduler());
    let mut session = PtyTerminalImageState::new(Arc::clone(&policy));
    let kitty_images = count_images(&mut session, &kitty_rgba(1, 1, &[1, 2, 3, 4]))?;
    let sixel_images = count_images(&mut session, SIXEL_FIXTURE)?;
    let observed = metrics(&scheduler)?;
    if kitty_images != 1 || sixel_images != 1 {
        return Err(format!("production decodes drifted: {kitty_images} {sixel_images}"));
    }
    if observed.admitted != 2 || observed.released != 2 {
        return Err(format!("production admissions drifted: {observed:?}"));
    }
    session.release_retained_storage();
    let (process_counters, session_counters) = session
        .storage_counters()
        .map_err(|error| format!("production storage counters: {error:?}"))?;
    let released = metrics(&scheduler)?;
    Ok((
        ProductionEvidence {
            kitty_images,
            sixel_images,
            production_admitted: observed.admitted,
            production_released: observed.released,
            released_exactly_once: (observed.admitted == observed.released).into(),
            queued_after: observed.queued,
            active_after: observed.active,
        },
        OwnershipEvidence {
            queued: released.queued,
            active: released.active,
            admitted: released.admitted,
            released: released.released,
            session_requested_current: session_counters.requested_current,
            process_requested_current: process_counters.requested_current,
        },
    ))
}

/// Every foreign capability is refused, and no storage is charged by the
/// refusal itself.
// @lat: [[test#Test Harness#Mandatory Decode Scheduling#Capability Refusal]]
fn verify_capabilities() -> Result<CapabilityEvidence, String> {
    let alpha = DecodeScheduler::new(ceilings(1, 4, Duration::from_secs(5)));
    let beta = DecodeScheduler::new(ceilings(1, 4, Duration::from_secs(5)));
    let storage = decode_storage();
    let other_storage = decode_storage();
    let session = alpha.new_session();
    let target = DecodeTarget::kitty(7);
    let base = request(session, target, &storage);

    let foreign_ticket = beta
        .issue(request(beta.new_session(), target, &storage))
        .map_err(|error| format!("foreign ticket: {error}"))?;
    let foreign_ticket_issuer = match alpha.admit(foreign_ticket) {
        Err(error) => error.name(),
        Ok(_) => return Err("a foreign scheduler admitted another issuer's ticket".to_owned()),
    };

    let permit = hold(&alpha, base.clone()).map_err(|error| format!("admit: {error}"))?;
    let refuse = |request: DecodeRequest| -> Result<&'static str, String> {
        match permit.authorize(&alpha, &request) {
            Err(error) => Ok(error.name()),
            Ok(()) => Err("a foreign capability was authorized".to_owned()),
        }
    };
    let foreign_issuer = match permit.authorize(&beta, &base) {
        Err(error) => error.name(),
        Ok(()) => return Err("a foreign issuer was authorized".to_owned()),
    };
    let foreign_session = refuse(DecodeRequest { session: alpha.new_session(), ..base.clone() })?;
    let foreign_generation = refuse(DecodeRequest { generation: 2, ..base.clone() })?;
    let foreign_target = refuse(DecodeRequest { target: DecodeTarget::sixel(7), ..base.clone() })?;
    let foreign_budget =
        refuse(DecodeRequest { storage: Arc::clone(&other_storage), ..base.clone() })?;
    let foreign_budget_bytes = refuse(DecodeRequest { requested_bytes: 5, ..base.clone() })?;

    let oversized = DecodeRequest { requested_bytes: (1 << 20) + 1, ..base };
    let request_exceeds_ceiling = match alpha.issue(oversized) {
        Err(error) => error.name(),
        Ok(_) => return Err("an oversized request was queued".to_owned()),
    };

    let (process, session_counters) = storage.validation_counters();
    let rejected_before_work = process.requested_current == 0
        && process.observed_current == 0
        && session_counters.requested_current == 0
        && session_counters.observed_current == 0
        && session_counters.allocator_attempts == 0;
    drop(permit);
    Ok(CapabilityEvidence {
        foreign_issuer,
        foreign_ticket_issuer,
        foreign_session,
        foreign_generation,
        foreign_target,
        foreign_budget,
        foreign_budget_bytes,
        request_exceeds_ceiling,
        rejected_before_work: rejected_before_work.into(),
    })
}

/// Four tickets issued in order are admitted in that order even though their
/// waiting threads race.
// @lat: [[test#Test Harness#Mandatory Decode Scheduling#FIFO Admission]]
fn verify_fifo() -> Result<FifoEvidence, String> {
    let scheduler = DecodeScheduler::new(ceilings(1, 8, Duration::from_secs(30)));
    let storage = decode_storage();
    let session = scheduler.new_session();
    let holder = hold(&scheduler, request(session, DecodeTarget::kitty(0), &storage))
        .map_err(|error| format!("holder: {error}"))?;

    let order = Arc::new(Mutex::new(Vec::new()));
    let mut issue_order = Vec::new();
    let mut workers = Vec::new();
    for index in 1..=4u64 {
        let ticket = scheduler
            .issue(request(session, DecodeTarget::kitty(index), &storage))
            .map_err(|error| format!("issue {index}: {error}"))?;
        issue_order.push(ticket.id());
        let scheduler = Arc::clone(&scheduler);
        let order = Arc::clone(&order);
        workers.push(thread::spawn(move || -> Result<(), String> {
            let id = ticket.id();
            let permit = scheduler.admit(ticket).map_err(|error| format!("admit: {error}"))?;
            let mut order = order.lock().map_err(|_| "order lock poisoned".to_owned())?;
            order.push(id);
            drop(order);
            drop(permit);
            Ok(())
        }));
    }
    drop(holder);
    for worker in workers {
        worker.join().map_err(|_| "FIFO worker panicked".to_owned())??;
    }
    let admission_order = order.lock().map_err(|_| "order lock poisoned".to_owned())?.clone();
    Ok(FifoEvidence {
        barged: (admission_order != issue_order).into(),
        issue_order,
        admission_order,
    })
}

/// Cancelling one target retires that waiter, wakes the next one, and reaches
/// in-flight decode work without touching an unrelated target.
// @lat: [[test#Test Harness#Mandatory Decode Scheduling#Cancellation and Deadline Retirement]]
fn verify_cancellation() -> Result<CancellationEvidence, String> {
    let scheduler = DecodeScheduler::new(ceilings(1, 8, Duration::from_secs(30)));
    let storage = decode_storage();
    let session = scheduler.new_session();
    let cancelled_target = DecodeTarget::kitty(11);
    let successor_target = DecodeTarget::kitty(12);
    let holder = hold(&scheduler, request(session, DecodeTarget::kitty(10), &storage))
        .map_err(|error| format!("holder: {error}"))?;

    let cancelled_ticket = scheduler
        .issue(request(session, cancelled_target, &storage))
        .map_err(|error| format!("cancelled ticket: {error}"))?;
    let successor_ticket = scheduler
        .issue(request(session, successor_target, &storage))
        .map_err(|error| format!("successor ticket: {error}"))?;

    let cancelled_worker = {
        let scheduler = Arc::clone(&scheduler);
        thread::spawn(move || scheduler.admit(cancelled_ticket).map(|permit| permit.id()))
    };
    let successor_worker = {
        let scheduler = Arc::clone(&scheduler);
        thread::spawn(move || {
            scheduler.admit(successor_ticket).map(|permit| (permit.id(), permit.is_cancelled()))
        })
    };
    let cancelled_count = scheduler
        .cancel_target(session, cancelled_target)
        .map_err(|error| format!("cancel: {error}"))?;
    if cancelled_count != 1 {
        return Err(format!("cancellation reached {cancelled_count} entries, expected 1"));
    }
    let cancelled_waiter = match cancelled_worker.join() {
        Ok(Err(error)) => error.name(),
        Ok(Ok(_)) => return Err("a cancelled waiter was admitted".to_owned()),
        Err(_) => return Err("cancelled worker panicked".to_owned()),
    };
    let unrelated_target_untouched = !holder.is_cancelled();
    drop(holder);
    let (successor_admitted, successor_not_cancelled) = match successor_worker.join() {
        Ok(Ok((_, cancelled))) => (true, !cancelled),
        Ok(Err(error)) => return Err(format!("successor refused: {error}")),
        Err(_) => return Err("successor worker panicked".to_owned()),
    };

    let in_flight = hold(&scheduler, request(session, DecodeTarget::sixel(21), &storage))
        .map_err(|error| format!("in-flight: {error}"))?;
    let bystander = decode_storage();
    let bystander_session = scheduler.new_session();
    let bystander_reach = scheduler
        .cancel_target(bystander_session, DecodeTarget::sixel(21))
        .map_err(|error| format!("bystander cancel: {error}"))?;
    if bystander_reach != 0 {
        return Err(format!("a foreign session cancelled {bystander_reach} entries"));
    }
    let bystander_untouched = !in_flight.is_cancelled();
    drop(bystander);
    let in_flight_reach = scheduler
        .cancel_target(session, DecodeTarget::sixel(21))
        .map_err(|error| format!("in-flight cancel: {error}"))?;
    if in_flight_reach != 1 {
        return Err(format!("in-flight cancellation reached {in_flight_reach} entries"));
    }
    let in_flight_cancelled = in_flight.is_cancelled();
    let in_flight_decode_refused = matches!(
        DecodeBudget::new(decode_limits(), &NoopHooks, &in_flight),
        Err(scribe_image_decode::BudgetError::DecodeCancelled { .. })
    );
    drop(in_flight);

    let observed = metrics(&scheduler)?;
    Ok(CancellationEvidence {
        cancelled_waiter,
        successor_admitted: successor_admitted.into(),
        successor_not_cancelled: successor_not_cancelled.into(),
        in_flight_cancelled: in_flight_cancelled.into(),
        in_flight_decode_refused: in_flight_decode_refused.into(),
        unrelated_target_untouched: (unrelated_target_untouched && bystander_untouched).into(),
        released_exactly_once: (observed.admitted == observed.released
            && observed.queued == 0
            && observed.active == 0)
            .into(),
    })
}

/// A waiter that outlives its queue-wait deadline retires itself and leaves
/// the queue immediately usable by the next request.
fn verify_deadline() -> Result<DeadlineEvidence, String> {
    let scheduler = DecodeScheduler::new(ceilings(1, 8, Duration::from_millis(120)));
    let storage = decode_storage();
    let session = scheduler.new_session();
    let holder = hold(&scheduler, request(session, DecodeTarget::kitty(30), &storage))
        .map_err(|error| format!("holder: {error}"))?;
    let ticket = scheduler
        .issue(request(session, DecodeTarget::kitty(31), &storage))
        .map_err(|error| format!("expiring ticket: {error}"))?;
    let expired_waiter = match scheduler.admit(ticket) {
        Err(error) => error.name(),
        Ok(_) => return Err("an expired waiter was admitted".to_owned()),
    };
    let expired = metrics(&scheduler)?;
    drop(holder);
    let successor = hold(&scheduler, request(session, DecodeTarget::kitty(32), &storage));
    let successor_admitted_after_release = successor.is_ok();
    drop(successor);
    let drained = metrics(&scheduler)?;
    Ok(DeadlineEvidence {
        expired_waiter,
        expired_total: expired.expired,
        successor_admitted_after_release: successor_admitted_after_release.into(),
        queued_after: drained.queued,
    })
}

/// Queue metadata never grows past the immutable depth ceiling, and an
/// abandoned ticket is pruned rather than parked forever.
// @lat: [[test#Test Harness#Mandatory Decode Scheduling#Bounded Queue Metadata]]
fn verify_bounded() -> Result<BoundedEvidence, String> {
    let scheduler = DecodeScheduler::new(ceilings(1, 2, Duration::from_secs(5)));
    let storage = decode_storage();
    let session = scheduler.new_session();
    let holder = hold(&scheduler, request(session, DecodeTarget::kitty(40), &storage))
        .map_err(|error| format!("holder: {error}"))?;
    let first = scheduler
        .issue(request(session, DecodeTarget::kitty(41), &storage))
        .map_err(|error| format!("first waiter: {error}"))?;
    let second = scheduler
        .issue(request(session, DecodeTarget::kitty(42), &storage))
        .map_err(|error| format!("second waiter: {error}"))?;
    let queue_full = match scheduler.issue(request(session, DecodeTarget::kitty(43), &storage)) {
        Err(error) => error.name(),
        Ok(_) => return Err("a third waiter entered a two-slot queue".to_owned()),
    };
    let saturated = metrics(&scheduler)?;
    drop(first);
    drop(second);
    let pruned = metrics(&scheduler)?;
    drop(holder);
    Ok(BoundedEvidence {
        queue_depth_ceiling: scheduler.ceilings().queue_depth,
        queue_full,
        peak_queued: saturated.peak_queued,
        abandoned_pruned: pruned.abandoned,
        queued_after_abandon: pruned.queued,
    })
}

/// One session holding a decode slot cannot stop an unrelated session's
/// production decode from completing.
fn verify_independent_session() -> Result<IndependentSessionEvidence, String> {
    let policy = TerminalImageProcessPolicy::with_decode_ceilings_for_validation(ceilings(
        2,
        8,
        Duration::from_secs(5),
    ));
    let scheduler = Arc::clone(policy.decode_scheduler());
    let storage = decode_storage();
    let holder_session = scheduler.new_session();
    let holder = hold(&scheduler, request(holder_session, DecodeTarget::kitty(50), &storage))
        .map_err(|error| format!("holder: {error}"))?;
    let mut progressing = PtyTerminalImageState::new(policy);
    let images = count_images(&mut progressing, SIXEL_FIXTURE)?
        + count_images(&mut progressing, &kitty_rgba(1, 1, &[9, 9, 9, 9]))?;
    let progressing_session = progressing.decode_session().get();
    progressing.release_retained_storage();
    drop(holder);
    Ok(IndependentSessionEvidence {
        holder_session: holder_session.get(),
        progressing_session,
        progressed: (images == 2).into(),
        images,
    })
}

fn decode_limits() -> DecodeLimits {
    DecodeLimits {
        max_width_pixels: 16,
        max_height_pixels: 16,
        max_pixels: 256,
        max_rgba_bytes: 1024,
        max_work_units: 1024,
        deadline: std::time::Instant::now() + Duration::from_secs(5),
        check_interval_work_units: 64,
    }
}

pub fn run(evidence_path: &Path) -> Result<(), String> {
    let (production, final_ownership) = verify_production()?;
    let capabilities = verify_capabilities()?;
    let fifo = verify_fifo()?;
    let cancellation = verify_cancellation()?;
    let deadline = verify_deadline()?;
    let bounded = verify_bounded()?;
    let independent_session = verify_independent_session()?;

    let failures = [
        (!production.released_exactly_once.0, "production admissions leaked ownership"),
        (
            production.queued_after != 0 || production.active_after != 0,
            "production left queue state",
        ),
        (capabilities.foreign_issuer != "foreign_issuer", "foreign issuer accepted"),
        (capabilities.foreign_ticket_issuer != "foreign_issuer", "foreign ticket accepted"),
        (capabilities.foreign_session != "foreign_session", "foreign session accepted"),
        (capabilities.foreign_generation != "foreign_generation", "foreign generation accepted"),
        (capabilities.foreign_target != "foreign_target", "foreign target accepted"),
        (capabilities.foreign_budget != "foreign_budget", "foreign budget accepted"),
        (capabilities.foreign_budget_bytes != "foreign_budget", "foreign budget size accepted"),
        (
            capabilities.request_exceeds_ceiling != "request_exceeds_ceiling",
            "oversized request accepted",
        ),
        (!capabilities.rejected_before_work.0, "a refused admission charged storage"),
        (fifo.barged.0, "admission order did not follow issue order"),
        (cancellation.cancelled_waiter != "cancelled", "cancelled waiter was not retired"),
        (!cancellation.successor_admitted.0, "cancellation did not wake the successor"),
        (!cancellation.successor_not_cancelled.0, "cancellation reached an unrelated waiter"),
        (!cancellation.in_flight_cancelled.0, "cancellation did not reach in-flight work"),
        (!cancellation.in_flight_decode_refused.0, "a cancelled permit still opened a budget"),
        (!cancellation.unrelated_target_untouched.0, "cancellation reached an unrelated target"),
        (!cancellation.released_exactly_once.0, "cancellation leaked ownership"),
        (deadline.expired_waiter != "deadline_expired", "expired waiter was not retired"),
        (deadline.expired_total != 1, "deadline retirement was not counted once"),
        (!deadline.successor_admitted_after_release.0, "deadline retirement blocked the queue"),
        (deadline.queued_after != 0, "deadline retirement left queue metadata"),
        (bounded.queue_full != "queue_full", "queue depth ceiling was not enforced"),
        (bounded.peak_queued > bounded.queue_depth_ceiling, "queue exceeded its depth ceiling"),
        (bounded.abandoned_pruned != 2, "abandoned tickets were not pruned"),
        (bounded.queued_after_abandon != 0, "abandoned tickets stayed queued"),
        (!independent_session.progressed.0, "an unrelated session could not progress"),
        (final_ownership.queued != 0 || final_ownership.active != 0, "scheduler state leaked"),
        (final_ownership.admitted != final_ownership.released, "admissions were not released"),
        (
            final_ownership.session_requested_current != 0
                || final_ownership.process_requested_current != 0,
            "retained storage leaked",
        ),
    ];
    if let Some((_, reason)) = failures.iter().find(|(failed, _)| *failed) {
        return Err((*reason).to_owned());
    }

    let evidence = Evidence {
        schema_version: 1,
        status: "pass",
        engine: "production session terminal seam",
        admission: "mandatory",
        production,
        capabilities,
        fifo,
        cancellation,
        deadline,
        bounded,
        independent_session,
        final_ownership,
    };
    write_evidence(evidence_path, &evidence)
}

fn write_evidence(evidence_path: &Path, evidence: &Evidence) -> Result<(), String> {
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| format!("encode scheduler evidence: {error}"))?;
    std::fs::write(evidence_path, bytes)
        .map_err(|error| format!("write {}: {error}", evidence_path.display()))
}
