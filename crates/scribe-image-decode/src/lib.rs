//! Shared cooperative controls for untrusted terminal-image decoders.

mod scheduler;

pub use scheduler::{
    DecodeAdmissionError, DecodeCeilings, DecodePermit, DecodeProtocol, DecodeRequest,
    DecodeScheduler, DecodeSchedulerMetrics, DecodeSessionId, DecodeTarget, DecodeTicket,
};

use std::error::Error;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard};
use std::time::Instant;

/// Number of distinct paired-ledger storage classes.
const CLASS_COUNT: usize = 9;

/// Stable allocation classes shared by the production image decoders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeAllocationClass {
    KittyBase64,
    KittyInflate,
    KittyRgba,
    PngInflate,
    PngRgba,
    SixelRgba,
}

/// Exact storage-ledger failures preserved across decoder crate boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeStorageError {
    SessionLimit,
    ProcessLimit,
    CounterOverflow,
    AllocationFailed,
    InternalInvariant,
}

/// All hostile-input-proportional storage classes sharing one paired ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageClass {
    FramingCandidate,
    FramingActive,
    FramingEvents,
    TerminalOutputs,
    CanonicalSixel,
    DecodedKitty,
    DecodedSixel,
    GridObservations,
    CanonicalMutations,
}

impl StorageClass {
    const fn index(self) -> usize {
        match self {
            Self::FramingCandidate => 0,
            Self::FramingActive => 1,
            Self::FramingEvents => 2,
            Self::TerminalOutputs => 3,
            Self::CanonicalSixel => 4,
            Self::DecodedKitty => 5,
            Self::DecodedSixel => 6,
            Self::GridObservations => 7,
            Self::CanonicalMutations => 8,
        }
    }
}

impl DecodeAllocationClass {
    const fn storage_class(self) -> StorageClass {
        match self {
            Self::KittyBase64
            | Self::KittyInflate
            | Self::KittyRgba
            | Self::PngInflate
            | Self::PngRgba => StorageClass::DecodedKitty,
            Self::SixelRgba => StorageClass::DecodedSixel,
        }
    }
}

/// Requested and allocator-observed paired-ledger telemetry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageCounters {
    pub requested_current: u64,
    pub requested_peak: u64,
    pub observed_current: u64,
    pub observed_peak: u64,
    pub reservation_attempts: u64,
    pub allocator_attempts: u64,
    pub reserve_before_allocation_calls: u64,
    pub observed_reconciliations: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageClassCounters {
    pub requested_current: u64,
    pub requested_peak: u64,
    pub observed_current: u64,
    pub observed_peak: u64,
}

#[derive(Debug, Default)]
struct LedgerState {
    counters: StorageCounters,
    classes: [StorageClassCounters; CLASS_COUNT],
    invariant_failed: bool,
    validation_poisoned: bool,
}

#[derive(Debug)]
struct Ledger {
    maximum: u64,
    state: Mutex<LedgerState>,
}

impl Ledger {
    fn new(maximum: u64) -> Self {
        Self { maximum, state: Mutex::new(LedgerState::default()) }
    }

    fn lock(&self) -> Result<MutexGuard<'_, LedgerState>, DecodeStorageError> {
        self.state.lock().map_err(|_| DecodeStorageError::InternalInvariant)
    }
}

/// Shared process half of concrete image storage accounting.
#[derive(Debug)]
pub struct StorageProcess {
    ledger: Arc<Ledger>,
    transaction_gate: Arc<Mutex<()>>,
}

impl StorageProcess {
    #[must_use]
    pub fn new(maximum: u64) -> Arc<Self> {
        Arc::new(Self {
            ledger: Arc::new(Ledger::new(maximum)),
            transaction_gate: Arc::new(Mutex::new(())),
        })
    }
}

/// Immutable validation-only ledger initialization fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageLedgerValidationFault {
    ReservationAttemptOverflow,
    ReserveCallOverflow,
    AllocatorAttemptOverflow,
    ReconciliationOverflow,
    RequestedCounterOverflow,
    ObservedCounterOverflow,
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageLedgerScope {
    Process,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageLedgerOperation {
    Reserve,
    Reconcile,
}

#[derive(Clone, Copy, Debug)]
pub struct StorageSnapshotValidationFault {
    pub operation: StorageLedgerOperation,
    pub scope: StorageLedgerScope,
    pub rejection: DecodeStorageError,
}

#[derive(Clone, Debug)]
pub struct StorageValidationPause {
    pub class: StorageClass,
    pub matching_ordinal: u64,
    pub reached: Arc<Barrier>,
    pub resume: Arc<Barrier>,
}

#[derive(Clone, Copy, Debug)]
pub struct StorageValidationRejection {
    pub class: StorageClass,
    pub matching_ordinal: u64,
    pub rejection: DecodeStorageError,
}

#[derive(Clone, Debug, Default)]
pub struct StorageValidation {
    pub ledger_fault: Option<StorageLedgerValidationFault>,
    pub rejection: Option<StorageValidationRejection>,
    pub snapshot_fault: Option<StorageSnapshotValidationFault>,
    pub pause: Option<StorageValidationPause>,
}

#[derive(Debug)]
struct ReconcileTelemetry {
    occurrences: [AtomicU64; CLASS_COUNT],
    rejection: Mutex<Option<(StorageClass, u64)>>,
}

impl Default for ReconcileTelemetry {
    fn default() -> Self {
        Self {
            occurrences: std::array::from_fn(|_| AtomicU64::new(0)),
            rejection: Mutex::new(None),
        }
    }
}

/// Concrete, non-cloneable session/process storage capability.
#[derive(Debug)]
pub struct DecodeStorage {
    session: Arc<Ledger>,
    process: Arc<StorageProcess>,
    observed_capacity_extra: usize,
    validation: StorageValidation,
    matching_reservations: AtomicU64,
    rejection_fired: AtomicU64,
    staged_allocations: AtomicU64,
    reconcile_telemetry: Arc<ReconcileTelemetry>,
}

impl DecodeStorage {
    #[must_use]
    pub fn new(
        process: Arc<StorageProcess>,
        session_maximum: u64,
        observed_capacity_extra: usize,
        validation: StorageValidation,
    ) -> Arc<Self> {
        let session = Arc::new(Ledger::new(session_maximum));
        if let Some(fault) = validation.ledger_fault {
            let mut state = session.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match fault {
                StorageLedgerValidationFault::ReservationAttemptOverflow => {
                    state.counters.reservation_attempts = u64::MAX;
                }
                StorageLedgerValidationFault::ReserveCallOverflow => {
                    state.counters.reserve_before_allocation_calls = u64::MAX;
                }
                StorageLedgerValidationFault::AllocatorAttemptOverflow => {
                    state.counters.allocator_attempts = u64::MAX;
                }
                StorageLedgerValidationFault::ReconciliationOverflow => {
                    state.counters.observed_reconciliations = u64::MAX;
                }
                StorageLedgerValidationFault::RequestedCounterOverflow => {
                    state.counters.requested_current = u64::MAX;
                    state.counters.requested_peak = u64::MAX;
                }
                StorageLedgerValidationFault::ObservedCounterOverflow => {
                    state.counters.observed_current = u64::MAX;
                    state.counters.observed_peak = u64::MAX;
                }
                StorageLedgerValidationFault::Poisoned => state.validation_poisoned = true,
            }
        }
        Arc::new(Self {
            session,
            process,
            observed_capacity_extra,
            validation,
            matching_reservations: AtomicU64::new(0),
            rejection_fired: AtomicU64::new(0),
            staged_allocations: AtomicU64::new(0),
            reconcile_telemetry: Arc::new(ReconcileTelemetry::default()),
        })
    }

    pub fn reserve(
        &self,
        class: StorageClass,
        requested_bytes: usize,
    ) -> Result<DecodeStorageLease, DecodeStorageError> {
        self.validate_reservation(class)?;
        let requested =
            u64::try_from(requested_bytes).map_err(|_| DecodeStorageError::CounterOverflow)?;
        let mut process = self.process.ledger.lock()?;
        let mut session = self.session.lock()?;
        paired_results(healthy(&process), healthy(&session))?;
        let (process_attempt, session_attempt) = paired_results(
            reservation_attempt(process.counters),
            reservation_attempt(session.counters),
        )?;
        let process_next = validation_result(
            self.validation.snapshot_fault,
            StorageLedgerOperation::Reserve,
            StorageLedgerScope::Process,
            requested_charge(process_attempt, self.process.ledger.maximum, requested),
        );
        let session_next = validation_result(
            self.validation.snapshot_fault,
            StorageLedgerOperation::Reserve,
            StorageLedgerScope::Session,
            requested_charge(session_attempt, self.session.maximum, requested),
        );
        let (process_next, session_next) = paired_results(process_next, session_next)?;
        let (process_class, session_class) = paired_results(
            class_charge(class_counter(&process.classes, class)?, requested),
            class_charge(class_counter(&session.classes, class)?, requested),
        )?;
        process.counters = process_next;
        session.counters = session_next;
        *class_counter_mut(&mut process.classes, class)? = process_class;
        *class_counter_mut(&mut session.classes, class)? = session_class;
        Ok(DecodeStorageLease {
            session: Arc::clone(&self.session),
            process: Arc::clone(&self.process.ledger),
            requested,
            observed: requested,
            released: false,
            snapshot_fault: self.validation.snapshot_fault,
            class,
            reconcile_telemetry: Arc::clone(&self.reconcile_telemetry),
        })
    }

    fn validate_reservation(&self, class: StorageClass) -> Result<(), DecodeStorageError> {
        let Some(target) = self.validation.rejection.filter(|target| target.class == class) else {
            return Ok(());
        };
        let ordinal = self
            .matching_reservations
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
            .map_err(|_| DecodeStorageError::CounterOverflow)?
            .saturating_add(1);
        if ordinal != target.matching_ordinal {
            return Ok(());
        }
        self.rejection_fired.fetch_add(1, Ordering::Relaxed);
        self.pause_validation_reservation(class, ordinal);
        Err(target.rejection)
    }

    fn pause_validation_reservation(&self, class: StorageClass, ordinal: u64) {
        let Some(pause) = self
            .validation
            .pause
            .as_ref()
            .filter(|pause| pause.class == class && pause.matching_ordinal == ordinal)
        else {
            return;
        };
        pause.reached.wait();
        pause.resume.wait();
    }

    pub fn reserve_decode(
        &self,
        class: DecodeAllocationClass,
        requested_bytes: usize,
    ) -> Result<DecodeStorageLease, DecodeStorageError> {
        self.reserve(class.storage_class(), requested_bytes)
    }

    pub fn observe_allocation_capacity(
        &self,
        allocated_capacity: usize,
    ) -> Result<usize, DecodeStorageError> {
        allocated_capacity
            .checked_add(self.observed_capacity_extra)
            .ok_or(DecodeStorageError::CounterOverflow)
    }

    pub fn counters(&self) -> Result<(StorageCounters, StorageCounters), DecodeStorageError> {
        let process = self.process.ledger.lock()?;
        let session = self.session.lock()?;
        healthy(&process)?;
        healthy(&session)?;
        Ok((session.counters, process.counters))
    }

    pub fn class_counters(
        &self,
        class: StorageClass,
    ) -> Result<(StorageClassCounters, StorageClassCounters), DecodeStorageError> {
        let process = self.process.ledger.lock()?;
        let session = self.session.lock()?;
        healthy(&process)?;
        healthy(&session)?;
        Ok((class_counter(&session.classes, class)?, class_counter(&process.classes, class)?))
    }

    #[must_use]
    pub fn validation_counters(&self) -> (StorageCounters, StorageCounters) {
        let process =
            self.process.ledger.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = self.session.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (session.counters, process.counters)
    }

    #[must_use]
    pub fn validation_rejection_observation(&self) -> (u64, u64, u64) {
        (
            self.matching_reservations.load(Ordering::Relaxed),
            self.rejection_fired.load(Ordering::Relaxed),
            self.staged_allocations.load(Ordering::Relaxed),
        )
    }

    #[must_use]
    pub fn validation_reconcile_rejection(&self) -> Option<(StorageClass, u64)> {
        *self
            .reconcile_telemetry
            .rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn record_validation_stage(&self, class: StorageClass) {
        if self.validation.rejection.is_some_and(|target| target.class == class) {
            self.staged_allocations.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn checkpoint(&self) -> Result<StorageCheckpoint, DecodeStorageError> {
        let (session, process) = self.counters()?;
        let process_state = self.process.ledger.lock()?;
        let session_state = self.session.lock()?;
        Ok(StorageCheckpoint {
            session,
            process,
            session_classes: session_state.classes,
            process_classes: process_state.classes,
        })
    }

    pub fn rollback(&self, checkpoint: &StorageCheckpoint) -> Result<(), DecodeStorageError> {
        let mut process = self.process.ledger.lock()?;
        let mut session = self.session.lock()?;
        paired_results(healthy(&process), healthy(&session))?;
        restore_peaks(&mut process.counters, checkpoint.process);
        restore_peaks(&mut session.counters, checkpoint.session);
        restore_class_peaks(&mut process.classes, &checkpoint.process_classes);
        restore_class_peaks(&mut session.classes, &checkpoint.session_classes);
        Ok(())
    }

    pub fn lock_transaction(&self) -> Result<MutexGuard<'_, ()>, DecodeStorageError> {
        self.process.transaction_gate.lock().map_err(|_| DecodeStorageError::InternalInvariant)
    }

    #[must_use]
    pub fn shares_process_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.process, &other.process)
    }
}

#[derive(Clone, Copy)]
pub struct StorageCheckpoint {
    session: StorageCounters,
    process: StorageCounters,
    session_classes: [StorageClassCounters; CLASS_COUNT],
    process_classes: [StorageClassCounters; CLASS_COUNT],
}

fn restore_peaks(counters: &mut StorageCounters, checkpoint: StorageCounters) {
    counters.requested_peak = checkpoint.requested_peak;
    counters.observed_peak = checkpoint.observed_peak;
}

fn restore_class_peaks(
    counters: &mut [StorageClassCounters; CLASS_COUNT],
    checkpoint: &[StorageClassCounters; CLASS_COUNT],
) {
    for (counter, saved) in counters.iter_mut().zip(checkpoint) {
        counter.requested_peak = saved.requested_peak;
        counter.observed_peak = saved.observed_peak;
    }
}

fn class_counter(
    counters: &[StorageClassCounters; CLASS_COUNT],
    class: StorageClass,
) -> Result<StorageClassCounters, DecodeStorageError> {
    counters.get(class.index()).copied().ok_or(DecodeStorageError::InternalInvariant)
}

fn class_counter_mut(
    counters: &mut [StorageClassCounters; CLASS_COUNT],
    class: StorageClass,
) -> Result<&mut StorageClassCounters, DecodeStorageError> {
    counters.get_mut(class.index()).ok_or(DecodeStorageError::InternalInvariant)
}

fn set_class_counter(
    counters: &mut [StorageClassCounters; CLASS_COUNT],
    class: StorageClass,
    value: StorageClassCounters,
) -> bool {
    let Some(counter) = counters.get_mut(class.index()) else {
        return false;
    };
    *counter = value;
    true
}

fn healthy(state: &LedgerState) -> Result<(), DecodeStorageError> {
    if state.invariant_failed || state.validation_poisoned {
        Err(DecodeStorageError::InternalInvariant)
    } else {
        Ok(())
    }
}

fn reservation_attempt(
    mut counters: StorageCounters,
) -> Result<StorageCounters, DecodeStorageError> {
    counters.reservation_attempts =
        counters.reservation_attempts.checked_add(1).ok_or(DecodeStorageError::CounterOverflow)?;
    Ok(counters)
}

fn requested_charge(
    mut counters: StorageCounters,
    maximum: u64,
    bytes: u64,
) -> Result<StorageCounters, DecodeStorageError> {
    let requested =
        counters.requested_current.checked_add(bytes).ok_or(DecodeStorageError::CounterOverflow)?;
    let observed =
        counters.observed_current.checked_add(bytes).ok_or(DecodeStorageError::CounterOverflow)?;
    if requested > maximum || observed > maximum {
        return Err(DecodeStorageError::SessionLimit);
    }
    counters.requested_current = requested;
    counters.observed_current = observed;
    counters.requested_peak = counters.requested_peak.max(requested);
    counters.observed_peak = counters.observed_peak.max(observed);
    counters.reserve_before_allocation_calls = counters
        .reserve_before_allocation_calls
        .checked_add(1)
        .ok_or(DecodeStorageError::CounterOverflow)?;
    Ok(counters)
}

fn class_charge(
    mut counters: StorageClassCounters,
    bytes: u64,
) -> Result<StorageClassCounters, DecodeStorageError> {
    counters.requested_current =
        counters.requested_current.checked_add(bytes).ok_or(DecodeStorageError::CounterOverflow)?;
    counters.observed_current =
        counters.observed_current.checked_add(bytes).ok_or(DecodeStorageError::CounterOverflow)?;
    counters.requested_peak = counters.requested_peak.max(counters.requested_current);
    counters.observed_peak = counters.observed_peak.max(counters.observed_current);
    Ok(counters)
}

fn paired_results<T>(
    process: Result<T, DecodeStorageError>,
    session: Result<T, DecodeStorageError>,
) -> Result<(T, T), DecodeStorageError> {
    let process_error = process.as_ref().err().copied();
    let session_error = session.as_ref().err().copied();
    for error in [process_error, session_error].into_iter().flatten() {
        if !matches!(error, DecodeStorageError::SessionLimit | DecodeStorageError::ProcessLimit) {
            return Err(error);
        }
    }
    if process_error.is_some() {
        return Err(DecodeStorageError::ProcessLimit);
    }
    if session_error.is_some() {
        return Err(DecodeStorageError::SessionLimit);
    }
    match (process, session) {
        (Ok(process), Ok(session)) => Ok((process, session)),
        _ => Err(DecodeStorageError::InternalInvariant),
    }
}

fn validation_result<T>(
    fault: Option<StorageSnapshotValidationFault>,
    operation: StorageLedgerOperation,
    scope: StorageLedgerScope,
    result: Result<T, DecodeStorageError>,
) -> Result<T, DecodeStorageError> {
    match fault {
        Some(fault) if fault.operation == operation && fault.scope == scope => Err(fault.rejection),
        _ => result,
    }
}

/// Move-only ownership issued before one concrete storage allocation.
pub struct DecodeStorageLease {
    session: Arc<Ledger>,
    process: Arc<Ledger>,
    requested: u64,
    observed: u64,
    released: bool,
    snapshot_fault: Option<StorageSnapshotValidationFault>,
    class: StorageClass,
    reconcile_telemetry: Arc<ReconcileTelemetry>,
}

impl DecodeStorageLease {
    pub fn record_allocation_attempt(&mut self) -> Result<(), DecodeStorageError> {
        let mut process = self.process.lock()?;
        let mut session = self.session.lock()?;
        paired_results(healthy(&process), healthy(&session))?;
        let (process_next, session_next) = paired_results(
            allocator_attempt(process.counters),
            allocator_attempt(session.counters),
        )?;
        process.counters = process_next;
        session.counters = session_next;
        Ok(())
    }

    pub fn reconcile_observed(&mut self, observed_bytes: usize) -> Result<(), DecodeStorageError> {
        let observed =
            u64::try_from(observed_bytes).map_err(|_| DecodeStorageError::CounterOverflow)?;
        if observed < self.requested || observed < self.observed {
            return Err(DecodeStorageError::InternalInvariant);
        }
        let occurrence = self
            .reconcile_telemetry
            .occurrences
            .get(self.class.index())
            .ok_or(DecodeStorageError::InternalInvariant)?
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let mut process = self.process.lock()?;
        let mut session = self.session.lock()?;
        paired_results(healthy(&process), healthy(&session))?;
        let process_next = validation_result(
            self.snapshot_fault,
            StorageLedgerOperation::Reconcile,
            StorageLedgerScope::Process,
            reconcile_snapshot(process.counters, self.process.maximum, self.observed, observed),
        );
        let session_next = validation_result(
            self.snapshot_fault,
            StorageLedgerOperation::Reconcile,
            StorageLedgerScope::Session,
            reconcile_snapshot(session.counters, self.session.maximum, self.observed, observed),
        );
        match paired_results(process_next, session_next) {
            Ok((process_next, session_next)) => {
                let (process_class, session_class) = paired_results(
                    class_reconcile(
                        class_counter(&process.classes, self.class)?,
                        self.observed,
                        observed,
                    ),
                    class_reconcile(
                        class_counter(&session.classes, self.class)?,
                        self.observed,
                        observed,
                    ),
                )?;
                process.counters = process_next;
                session.counters = session_next;
                *class_counter_mut(&mut process.classes, self.class)? = process_class;
                *class_counter_mut(&mut session.classes, self.class)? = session_class;
                self.observed = observed;
                Ok(())
            }
            Err(error) => {
                *self
                    .reconcile_telemetry
                    .rejection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((self.class, occurrence));
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        usize::try_from(self.requested).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn observed_bytes(&self) -> usize {
        usize::try_from(self.observed).unwrap_or(usize::MAX)
    }
}

fn allocator_attempt(mut counters: StorageCounters) -> Result<StorageCounters, DecodeStorageError> {
    counters.allocator_attempts =
        counters.allocator_attempts.checked_add(1).ok_or(DecodeStorageError::CounterOverflow)?;
    Ok(counters)
}

fn reconcile_snapshot(
    mut counters: StorageCounters,
    maximum: u64,
    previous: u64,
    observed: u64,
) -> Result<StorageCounters, DecodeStorageError> {
    let extra = observed.checked_sub(previous).ok_or(DecodeStorageError::InternalInvariant)?;
    let next =
        counters.observed_current.checked_add(extra).ok_or(DecodeStorageError::CounterOverflow)?;
    if next > maximum {
        return Err(DecodeStorageError::SessionLimit);
    }
    counters.observed_current = next;
    counters.observed_peak = counters.observed_peak.max(next);
    counters.observed_reconciliations = counters
        .observed_reconciliations
        .checked_add(1)
        .ok_or(DecodeStorageError::CounterOverflow)?;
    Ok(counters)
}

fn class_reconcile(
    mut counters: StorageClassCounters,
    previous: u64,
    observed: u64,
) -> Result<StorageClassCounters, DecodeStorageError> {
    let extra = observed.checked_sub(previous).ok_or(DecodeStorageError::InternalInvariant)?;
    counters.observed_current =
        counters.observed_current.checked_add(extra).ok_or(DecodeStorageError::CounterOverflow)?;
    counters.observed_peak = counters.observed_peak.max(counters.observed_current);
    Ok(counters)
}

impl Drop for DecodeStorageLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let Ok(mut process) = self.process.state.lock() else { return };
        let Ok(mut session) = self.session.state.lock() else {
            process.invariant_failed = true;
            return;
        };
        let process_next = release_snapshot(&process, self.requested, self.observed);
        let session_next = release_snapshot(&session, self.requested, self.observed);
        let process_class = class_counter(&process.classes, self.class)
            .ok()
            .and_then(|counters| class_release(counters, self.requested, self.observed));
        let session_class = class_counter(&session.classes, self.class)
            .ok()
            .and_then(|counters| class_release(counters, self.requested, self.observed));
        if let (Some(process_next), Some(session_next), Some(process_class), Some(session_class)) =
            (process_next, session_next, process_class, session_class)
        {
            process.counters = process_next;
            session.counters = session_next;
            let process_updated =
                set_class_counter(&mut process.classes, self.class, process_class);
            let session_updated =
                set_class_counter(&mut session.classes, self.class, session_class);
            if !process_updated || !session_updated {
                process.invariant_failed = true;
                session.invariant_failed = true;
            }
        } else {
            process.invariant_failed = true;
            session.invariant_failed = true;
        }
    }
}

fn release_snapshot(state: &LedgerState, requested: u64, observed: u64) -> Option<StorageCounters> {
    if state.invariant_failed {
        return None;
    }
    let mut counters = state.counters;
    counters.requested_current = counters.requested_current.checked_sub(requested)?;
    counters.observed_current = counters.observed_current.checked_sub(observed)?;
    Some(counters)
}

fn class_release(
    mut counters: StorageClassCounters,
    requested: u64,
    observed: u64,
) -> Option<StorageClassCounters> {
    counters.requested_current = counters.requested_current.checked_sub(requested)?;
    counters.observed_current = counters.observed_current.checked_sub(observed)?;
    Some(counters)
}

/// Decoder bytes whose allocation and allocator capacity retain their lease.
// @lat: [[terminal-images#Terminal Images#Exact Requested Storage Accounting]]
pub struct DecodeBuffer {
    bytes: Vec<u8>,
    lease: DecodeStorageLease,
}

impl DecodeBuffer {
    pub fn allocate(
        storage: &DecodeStorage,
        class: DecodeAllocationClass,
        requested: usize,
    ) -> Result<Self, DecodeStorageError> {
        let mut lease = storage.reserve_decode(class, requested)?;
        if requested != 0 {
            lease.record_allocation_attempt()?;
        }
        let mut bytes = Vec::new();
        if requested != 0 {
            bytes.try_reserve_exact(requested).map_err(|_| DecodeStorageError::AllocationFailed)?;
            let observed = storage.observe_allocation_capacity(bytes.capacity())?;
            lease.reconcile_observed(observed)?;
        }
        Ok(Self { bytes, lease })
    }

    #[must_use]
    pub fn requested_bytes(&self) -> usize {
        self.lease.requested_bytes()
    }

    #[must_use]
    pub fn observed_bytes(&self) -> usize {
        self.lease.observed_bytes()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn resize(&mut self, new_len: usize, value: u8) -> Result<(), DecodeStorageError> {
        if new_len > self.bytes.capacity() {
            return Err(DecodeStorageError::InternalInvariant);
        }
        self.bytes.resize(new_len, value);
        Ok(())
    }

    pub fn truncate(&mut self, new_len: usize) {
        self.bytes.truncate(new_len);
    }

    pub fn extend_from_slice(&mut self, source: &[u8]) -> Result<(), DecodeStorageError> {
        let new_len = self
            .bytes
            .len()
            .checked_add(source.len())
            .ok_or(DecodeStorageError::CounterOverflow)?;
        if new_len > self.bytes.capacity() {
            return Err(DecodeStorageError::InternalInvariant);
        }
        self.bytes.extend_from_slice(source);
        Ok(())
    }

    pub fn push(&mut self, value: u8) -> Result<(), DecodeStorageError> {
        if self.bytes.len() == self.bytes.capacity() {
            return Err(DecodeStorageError::InternalInvariant);
        }
        self.bytes.push(value);
        Ok(())
    }
}

impl fmt::Debug for DecodeBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DecodeBuffer")
            .field("len", &self.bytes.len())
            .field("capacity", &self.bytes.capacity())
            .field("requested_bytes", &self.requested_bytes())
            .field("observed_bytes", &self.observed_bytes())
            .finish_non_exhaustive()
    }
}

impl Deref for DecodeBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl DerefMut for DecodeBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}

impl PartialEq<[u8]> for DecodeBuffer {
    fn eq(&self, other: &[u8]) -> bool {
        self.bytes == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for DecodeBuffer {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.bytes == *other
    }
}

/// Caller-selected work, deadline, and observation limits for one decode.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_width_pixels: usize,
    pub max_height_pixels: usize,
    pub max_pixels: usize,
    pub max_rgba_bytes: usize,
    pub max_work_units: u64,
    pub deadline: Instant,
    pub check_interval_work_units: u64,
}

impl DecodeLimits {
    /// Frozen terminal-images-v1 limits with a caller-selected deadline.
    pub const fn terminal_images_v1(deadline: Instant) -> Self {
        Self {
            max_width_pixels: 4_096,
            max_height_pixels: 4_096,
            max_pixels: 16_777_216,
            max_rgba_bytes: 67_108_864,
            max_work_units: 134_217_728,
            deadline,
            check_interval_work_units: 4_096,
        }
    }

    pub const fn validate(self) -> Result<(), BudgetError> {
        if self.max_width_pixels == 0 || self.max_height_pixels == 0 {
            return Err(BudgetError::InvalidLimits);
        }
        if self.max_pixels == 0
            || self.max_rgba_bytes == 0
            || self.max_work_units == 0
            || self.check_interval_work_units == 0
        {
            return Err(BudgetError::InvalidLimits);
        }
        Ok(())
    }
}

/// Payload-free allocation denial from a caller hook.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocationDenied;

impl fmt::Display for AllocationDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("allocation denied")
    }
}

impl Error for AllocationDenied {}

/// Cooperative controls owned by the decode caller.
pub trait DecodeHooks {
    fn is_cancelled(&self) -> bool;

    fn before_allocation(&self, _requested_bytes: usize) -> Result<(), AllocationDenied> {
        Ok(())
    }
}

/// Default hooks for callers that need only deadline/work enforcement.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopHooks;

impl DecodeHooks for NoopHooks {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Shared payload-free failures at cooperative decode boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    InvalidLimits,
    WorkBudgetExceeded { requested: u64, maximum: u64 },
    DecodeDeadlineExceeded { work_units: u64 },
    DecodeCancelled { work_units: u64 },
    AllocationFailed { requested_bytes: usize },
    Storage(DecodeStorageError),
}

impl fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("invalid decode limits"),
            Self::WorkBudgetExceeded { requested, maximum } => {
                write!(formatter, "work budget exceeded: {requested} > {maximum}")
            }
            Self::DecodeDeadlineExceeded { work_units } => {
                write!(formatter, "decode deadline exceeded at {work_units} work units")
            }
            Self::DecodeCancelled { work_units } => {
                write!(formatter, "decode cancelled at {work_units} work units")
            }
            Self::AllocationFailed { requested_bytes } => {
                write!(formatter, "allocation failed for {requested_bytes} bytes")
            }
            Self::Storage(error) => write!(formatter, "decode storage failure: {error:?}"),
        }
    }
}

impl Error for BudgetError {}

/// Observable statistics for one completed or rejected decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeStats {
    pub work_units: u64,
    pub cooperative_checks: u64,
    pub peak_live_allocation_bytes: usize,
}

// @lat: [[terminal-images#Terminal Images#Shared Decode Budget]]
/// Caller-owned cumulative work, cancellation, deadline, and allocation state.
///
/// The scheduler permit is the only route to a session storage budget, which
/// is what makes decode admission mandatory rather than advisory.
pub struct DecodeBudget<'a> {
    limits: DecodeLimits,
    hooks: &'a dyn DecodeHooks,
    permit: &'a DecodePermit,
    work_units: u64,
    next_check: u64,
    checks: u64,
    live_allocation_bytes: usize,
    peak_live_allocation_bytes: usize,
}

impl<'a> DecodeBudget<'a> {
    pub fn new(
        limits: DecodeLimits,
        hooks: &'a impl DecodeHooks,
        permit: &'a DecodePermit,
    ) -> Result<Self, BudgetError> {
        limits.validate()?;
        let mut budget = Self {
            limits,
            hooks,
            permit,
            work_units: 0,
            next_check: limits.check_interval_work_units,
            checks: 0,
            live_allocation_bytes: 0,
            peak_live_allocation_bytes: 0,
        };
        budget.check_now()?;
        Ok(budget)
    }

    pub const fn limits(&self) -> DecodeLimits {
        self.limits
    }

    pub fn charge(&mut self, units: u64) -> Result<(), BudgetError> {
        let requested =
            self.work_units.checked_add(units).ok_or(BudgetError::WorkBudgetExceeded {
                requested: u64::MAX,
                maximum: self.limits.max_work_units,
            })?;
        if requested > self.limits.max_work_units {
            return Err(BudgetError::WorkBudgetExceeded {
                requested,
                maximum: self.limits.max_work_units,
            });
        }
        while self.next_check <= requested {
            self.work_units = self.next_check;
            self.check_now()?;
            self.next_check = self
                .next_check
                .checked_add(self.limits.check_interval_work_units)
                .ok_or(BudgetError::WorkBudgetExceeded {
                    requested,
                    maximum: self.limits.max_work_units,
                })?;
        }
        self.work_units = requested;
        Ok(())
    }

    pub fn check_now(&mut self) -> Result<(), BudgetError> {
        self.checks = self.checks.saturating_add(1);
        if self.permit.is_cancelled() || self.hooks.is_cancelled() {
            return Err(BudgetError::DecodeCancelled { work_units: self.work_units });
        }
        if Instant::now() >= self.limits.deadline {
            return Err(BudgetError::DecodeDeadlineExceeded { work_units: self.work_units });
        }
        Ok(())
    }

    pub fn begin_allocation(&mut self, bytes: usize) -> Result<(), BudgetError> {
        self.hooks
            .before_allocation(bytes)
            .map_err(|_| BudgetError::AllocationFailed { requested_bytes: bytes })?;
        self.live_allocation_bytes = self
            .live_allocation_bytes
            .checked_add(bytes)
            .ok_or(BudgetError::AllocationFailed { requested_bytes: bytes })?;
        self.peak_live_allocation_bytes =
            self.peak_live_allocation_bytes.max(self.live_allocation_bytes);
        Ok(())
    }

    /// Reserve, allocate, and reconcile one decoder buffer before mutation.
    pub fn allocate(
        &mut self,
        class: DecodeAllocationClass,
        bytes: usize,
    ) -> Result<DecodeBuffer, BudgetError> {
        self.begin_allocation(bytes)?;
        match DecodeBuffer::allocate(self.permit.storage(), class, bytes) {
            Ok(buffer) => Ok(buffer),
            Err(error) => {
                self.end_allocation(bytes);
                Err(BudgetError::Storage(error))
            }
        }
    }

    pub fn end_allocation(&mut self, bytes: usize) {
        self.live_allocation_bytes = self.live_allocation_bytes.saturating_sub(bytes);
    }

    pub const fn stats(&self) -> DecodeStats {
        DecodeStats {
            work_units: self.work_units,
            cooperative_checks: self.checks,
            peak_live_allocation_bytes: self.peak_live_allocation_bytes,
        }
    }
}
