//! Mandatory FIFO admission in front of every image decode entry point.
//!
//! A decode cannot start without a [`DecodePermit`], and a permit exists only
//! as the result of [`DecodeScheduler::admit`] consuming a [`DecodeTicket`]
//! the same scheduler issued. The permit owns the session's
//! [`DecodeStorage`] handle, so [`crate::DecodeBudget`] — the type every
//! decoder charges — is unreachable without passing process admission first.
//!
//! Admission is strict FIFO by issue order: a ticket is eligible only at the
//! head of the queue, so a later caller can never barge past an earlier one.
//! That makes `issue` and `admit` a pair — production callers must admit (or
//! drop) a ticket on the thread that issued it rather than parking it, because
//! a queued head that nobody is waiting on holds the line until it retires.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::DecodeStorage;

/// Process-unique identity of one image session, minted by the scheduler.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DecodeSessionId(u64);

impl DecodeSessionId {
    /// Payload-free identity for evidence and log lines.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Wire protocol whose decoder the admitted work belongs to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DecodeProtocol {
    Kitty,
    Sixel,
}

/// The transfer or target image one decode admission is bound to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DecodeTarget {
    pub protocol: DecodeProtocol,
    pub id: u64,
}

impl DecodeTarget {
    /// One Kitty transfer, keyed by its protocol image identifier.
    #[must_use]
    pub const fn kitty(id: u64) -> Self {
        Self { protocol: DecodeProtocol::Kitty, id }
    }

    /// One Sixel command, keyed by the output sequence it will occupy.
    #[must_use]
    pub const fn sixel(id: u64) -> Self {
        Self { protocol: DecodeProtocol::Sixel, id }
    }
}

/// Immutable process-owned decode ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeCeilings {
    pub concurrent_decodes: u32,
    pub queue_depth: u32,
    pub queue_bytes: u64,
    pub queue_wait: Duration,
}

/// Typed refusal raised before any decode work is charged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeAdmissionError {
    ForeignIssuer,
    ForeignSession,
    ForeignGeneration,
    ForeignTarget,
    ForeignBudget,
    RequestExceedsCeiling { requested_bytes: u64, maximum: u64 },
    QueueFull { depth: u32, maximum: u32 },
    Cancelled,
    DeadlineExpired,
    Poisoned,
}

impl DecodeAdmissionError {
    /// Stable payload-free discriminant for functional evidence.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ForeignIssuer => "foreign_issuer",
            Self::ForeignSession => "foreign_session",
            Self::ForeignGeneration => "foreign_generation",
            Self::ForeignTarget => "foreign_target",
            Self::ForeignBudget => "foreign_budget",
            Self::RequestExceedsCeiling { .. } => "request_exceeds_ceiling",
            Self::QueueFull { .. } => "queue_full",
            Self::Cancelled => "cancelled",
            Self::DeadlineExpired => "deadline_expired",
            Self::Poisoned => "poisoned",
        }
    }
}

impl fmt::Display for DecodeAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "decode admission refused: {}", self.name())
    }
}

impl Error for DecodeAdmissionError {}

/// The exact capabilities one decode admission is bound to.
#[derive(Clone, Debug)]
pub struct DecodeRequest {
    pub session: DecodeSessionId,
    pub generation: u64,
    pub target: DecodeTarget,
    pub requested_bytes: u64,
    pub storage: Arc<DecodeStorage>,
}

/// Payload-free scheduler counters for functional evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecodeSchedulerMetrics {
    pub issued: u64,
    pub admitted: u64,
    pub released: u64,
    pub rejected: u64,
    pub cancelled: u64,
    pub expired: u64,
    pub abandoned: u64,
    pub queued: u32,
    pub active: u32,
    pub peak_queued: u32,
    pub peak_active: u32,
}

#[derive(Clone, Debug)]
struct Entry {
    ticket: u64,
    session: DecodeSessionId,
    target: DecodeTarget,
    requested_bytes: u64,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct SchedulerState {
    waiters: VecDeque<Entry>,
    active: Vec<Entry>,
    active_bytes: u64,
    metrics: DecodeSchedulerMetrics,
}

/// Move the queue head into the active set, charging its bytes once.
fn activate_head(state: &mut SchedulerState) -> Option<()> {
    let entry = state.waiters.pop_front()?;
    state.active_bytes = state.active_bytes.saturating_add(entry.requested_bytes);
    state.active.push(entry);
    state.metrics.admitted = state.metrics.admitted.saturating_add(1);
    state.metrics.peak_active =
        state.metrics.peak_active.max(u32::try_from(state.active.len()).unwrap_or(u32::MAX));
    Some(())
}

impl SchedulerState {
    fn snapshot(&self) -> DecodeSchedulerMetrics {
        DecodeSchedulerMetrics {
            queued: u32::try_from(self.waiters.len()).unwrap_or(u32::MAX),
            active: u32::try_from(self.active.len()).unwrap_or(u32::MAX),
            ..self.metrics
        }
    }
}

/// Process-owned mandatory decode admission.
// @lat: [[terminal-images#Terminal Images#Mandatory Decode Scheduling]]
#[derive(Debug)]
pub struct DecodeScheduler {
    issuer: u64,
    ceilings: DecodeCeilings,
    next_ticket: AtomicU64,
    next_session: AtomicU64,
    state: Mutex<SchedulerState>,
    wake: Condvar,
}

static NEXT_ISSUER: AtomicU64 = AtomicU64::new(1);

impl DecodeScheduler {
    /// Construct one process-owned scheduler with immutable ceilings.
    #[must_use]
    pub fn new(ceilings: DecodeCeilings) -> Arc<Self> {
        Arc::new(Self {
            issuer: NEXT_ISSUER.fetch_add(1, Ordering::Relaxed),
            ceilings,
            next_ticket: AtomicU64::new(1),
            next_session: AtomicU64::new(1),
            state: Mutex::new(SchedulerState::default()),
            wake: Condvar::new(),
        })
    }

    /// Identity every ticket and permit this scheduler issued carries.
    #[must_use]
    pub const fn issuer(&self) -> u64 {
        self.issuer
    }

    /// The immutable ceilings this scheduler enforces.
    #[must_use]
    pub const fn ceilings(&self) -> DecodeCeilings {
        self.ceilings
    }

    /// Mint one process-unique session identity.
    pub fn new_session(&self) -> DecodeSessionId {
        DecodeSessionId(self.next_session.fetch_add(1, Ordering::Relaxed))
    }

    /// Payload-free counters, including live queue and active depth.
    pub fn metrics(&self) -> Result<DecodeSchedulerMetrics, DecodeAdmissionError> {
        let state = self.state.lock().map_err(|_| DecodeAdmissionError::Poisoned)?;
        Ok(state.snapshot())
    }

    /// Enqueue one capability-bound request and return its FIFO ticket.
    pub fn issue(
        self: &Arc<Self>,
        request: DecodeRequest,
    ) -> Result<DecodeTicket, DecodeAdmissionError> {
        let mut state = self.state.lock().map_err(|_| DecodeAdmissionError::Poisoned)?;
        if request.requested_bytes > self.ceilings.queue_bytes {
            state.metrics.rejected = state.metrics.rejected.saturating_add(1);
            return Err(DecodeAdmissionError::RequestExceedsCeiling {
                requested_bytes: request.requested_bytes,
                maximum: self.ceilings.queue_bytes,
            });
        }
        let depth = u32::try_from(state.waiters.len()).unwrap_or(u32::MAX);
        if depth >= self.ceilings.queue_depth {
            state.metrics.rejected = state.metrics.rejected.saturating_add(1);
            return Err(DecodeAdmissionError::QueueFull {
                depth,
                maximum: self.ceilings.queue_depth,
            });
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        state.waiters.push_back(Entry {
            ticket,
            session: request.session,
            target: request.target,
            requested_bytes: request.requested_bytes,
            cancelled: Arc::clone(&cancelled),
        });
        state.metrics.issued = state.metrics.issued.saturating_add(1);
        state.metrics.peak_queued =
            state.metrics.peak_queued.max(u32::try_from(state.waiters.len()).unwrap_or(u32::MAX));
        Ok(DecodeTicket {
            scheduler: Arc::clone(self),
            id: ticket,
            session: request.session,
            generation: request.generation,
            target: request.target,
            requested_bytes: request.requested_bytes,
            storage: request.storage,
            cancelled,
            deadline: Instant::now() + self.ceilings.queue_wait,
            retired: false,
        })
    }

    /// Wait for this ticket to reach the head of the queue and a free slot.
    pub fn admit(
        self: &Arc<Self>,
        ticket: DecodeTicket,
    ) -> Result<DecodePermit, DecodeAdmissionError> {
        if !Arc::ptr_eq(&ticket.scheduler, self) {
            return Err(DecodeAdmissionError::ForeignIssuer);
        }
        let mut ticket = ticket;
        let mut state = self.state.lock().map_err(|_| DecodeAdmissionError::Poisoned)?;
        loop {
            let Some(index) = state.waiters.iter().position(|entry| entry.ticket == ticket.id)
            else {
                ticket.retired = true;
                return Err(DecodeAdmissionError::Cancelled);
            };
            let cancelled = state
                .waiters
                .get(index)
                .is_some_and(|entry| entry.cancelled.load(Ordering::SeqCst));
            if cancelled {
                state.waiters.remove(index);
                state.metrics.cancelled = state.metrics.cancelled.saturating_add(1);
                ticket.retired = true;
                drop(state);
                self.wake.notify_all();
                return Err(DecodeAdmissionError::Cancelled);
            }
            if index == 0 && self.has_capacity(&state, ticket.requested_bytes) {
                ticket.retired = true;
                activate_head(&mut state).ok_or(DecodeAdmissionError::Poisoned)?;
                return Ok(self.permit_for(&ticket));
            }
            let now = Instant::now();
            if now >= ticket.deadline {
                state.waiters.remove(index);
                state.metrics.expired = state.metrics.expired.saturating_add(1);
                ticket.retired = true;
                drop(state);
                self.wake.notify_all();
                return Err(DecodeAdmissionError::DeadlineExpired);
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, ticket.deadline - now)
                .map_err(|_| DecodeAdmissionError::Poisoned)?;
            state = next;
        }
    }

    /// Whether one more decode of `bytes` fits inside the live ceilings.
    fn has_capacity(&self, state: &SchedulerState, bytes: u64) -> bool {
        u32::try_from(state.active.len()).unwrap_or(u32::MAX) < self.ceilings.concurrent_decodes
            && state.active_bytes.saturating_add(bytes) <= self.ceilings.queue_bytes
    }

    /// Mint the admission object for a ticket whose slot is already taken.
    fn permit_for(self: &Arc<Self>, ticket: &DecodeTicket) -> DecodePermit {
        DecodePermit {
            scheduler: Arc::clone(self),
            id: ticket.id,
            session: ticket.session,
            generation: ticket.generation,
            target: ticket.target,
            requested_bytes: ticket.requested_bytes,
            storage: Arc::clone(&ticket.storage),
            cancelled: Arc::clone(&ticket.cancelled),
        }
    }

    /// Cancel queued and in-flight work for exactly one session target.
    pub fn cancel_target(
        &self,
        session: DecodeSessionId,
        target: DecodeTarget,
    ) -> Result<usize, DecodeAdmissionError> {
        let state = self.state.lock().map_err(|_| DecodeAdmissionError::Poisoned)?;
        let matches = state
            .waiters
            .iter()
            .chain(state.active.iter())
            .filter(|entry| entry.session == session && entry.target == target);
        let mut cancelled = 0;
        for entry in matches {
            entry.cancelled.store(true, Ordering::SeqCst);
            cancelled += 1;
        }
        drop(state);
        if cancelled > 0 {
            self.wake.notify_all();
        }
        Ok(cancelled)
    }
}

/// FIFO position issued by one scheduler, bound to one set of capabilities.
#[derive(Debug)]
pub struct DecodeTicket {
    scheduler: Arc<DecodeScheduler>,
    id: u64,
    session: DecodeSessionId,
    generation: u64,
    target: DecodeTarget,
    requested_bytes: u64,
    storage: Arc<DecodeStorage>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    retired: bool,
}

impl DecodeTicket {
    /// Queue position identity, monotonic per scheduler.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Identity of the scheduler that issued this ticket.
    #[must_use]
    pub fn issuer(&self) -> u64 {
        self.scheduler.issuer
    }

    /// Session this ticket may decode for.
    #[must_use]
    pub const fn session(&self) -> DecodeSessionId {
        self.session
    }

    /// Transfer or target image this ticket may decode.
    #[must_use]
    pub const fn target(&self) -> DecodeTarget {
        self.target
    }
}

impl Drop for DecodeTicket {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        let Ok(mut state) = self.scheduler.state.lock() else { return };
        if let Some(index) = state.waiters.iter().position(|entry| entry.ticket == self.id) {
            state.waiters.remove(index);
            state.metrics.abandoned = state.metrics.abandoned.saturating_add(1);
        }
        drop(state);
        self.scheduler.wake.notify_all();
    }
}

/// Non-forgeable admission every decode entry point requires.
#[derive(Debug)]
pub struct DecodePermit {
    scheduler: Arc<DecodeScheduler>,
    id: u64,
    session: DecodeSessionId,
    generation: u64,
    target: DecodeTarget,
    requested_bytes: u64,
    storage: Arc<DecodeStorage>,
    cancelled: Arc<AtomicBool>,
}

impl DecodePermit {
    /// The session storage budget this admission may charge.
    #[must_use]
    pub fn storage(&self) -> &DecodeStorage {
        &self.storage
    }

    /// Whether the owning transfer or target image has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Admitted queue position identity.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Session this admission may decode for.
    #[must_use]
    pub const fn session(&self) -> DecodeSessionId {
        self.session
    }

    /// Transfer or target image this admission may decode.
    #[must_use]
    pub const fn target(&self) -> DecodeTarget {
        self.target
    }

    /// Reject a permit whose issuer, session, generation, target, or budget
    /// capability differs from the work about to run.
    pub fn authorize(
        &self,
        scheduler: &Arc<DecodeScheduler>,
        request: &DecodeRequest,
    ) -> Result<(), DecodeAdmissionError> {
        if !Arc::ptr_eq(&self.scheduler, scheduler) {
            return Err(DecodeAdmissionError::ForeignIssuer);
        }
        if self.session != request.session {
            return Err(DecodeAdmissionError::ForeignSession);
        }
        if self.generation != request.generation {
            return Err(DecodeAdmissionError::ForeignGeneration);
        }
        if self.target != request.target {
            return Err(DecodeAdmissionError::ForeignTarget);
        }
        if !Arc::ptr_eq(&self.storage, &request.storage)
            || self.requested_bytes != request.requested_bytes
        {
            return Err(DecodeAdmissionError::ForeignBudget);
        }
        Ok(())
    }
}

impl Drop for DecodePermit {
    fn drop(&mut self) {
        let Ok(mut state) = self.scheduler.state.lock() else { return };
        if let Some(index) = state.active.iter().position(|entry| entry.ticket == self.id) {
            let entry = state.active.remove(index);
            state.active_bytes = state.active_bytes.saturating_sub(entry.requested_bytes);
            state.metrics.released = state.metrics.released.saturating_add(1);
        }
        drop(state);
        self.scheduler.wake.notify_all();
    }
}
