//! Per-session exit funnel and PTY-reader cancellation (spec 017 US1-3).
//!
//! A session's PTY master fd is duplicated three ways — the reader's
//! `AsyncPtyFd`, the resize fd, and the `alacritty_terminal::tty::Pty` parked
//! on the `LiveSession` — so dropping any single one of them never delivers
//! EOF to the reader. A child that ignores SIGHUP (`trap '' HUP; sleep inf`)
//! therefore leaves the reader parked on a `read()` that can never complete,
//! and with its `JoinHandle` discarded teardown could neither stop nor bound
//! it.
//!
//! [`SessionExitGate`] closes both halves. It carries the reader's
//! cancellation signal plus its retained `JoinHandle`, and it owns the
//! compare-and-swap that elects exactly one exit path to publish
//! `SessionExited` and unwire the session. Every path that can end a session —
//! the reader's EOF/read error, an explicit `CloseSession`/`CloseWindow`, and
//! the child-exit watcher — arbitrates through that CAS, so no interleaving
//! can double-emit and none can leave the session unfinalized.
//!
//! # Close protocol and lock order (spec 017 US1-1)
//!
//! Every explicit close runs the same three steps, in this order:
//!
//! 1. **Take.** Under the `live_sessions` write guard — and only there —
//!    remove the registry entry and move the session's `PtyGuard` and exit
//!    gate out of it. Nothing inside that critical section may `.await`.
//! 2. **Release, then unwire.** Drop the guard before anything else, and only
//!    then take the `workspace_manager` write guard for the workspace-side
//!    removal. The two are never held at once, and neither is held while the
//!    child is signalled or reaped.
//! 3. **Cancel, tear down, join.** Raise [`SessionExitGate::cancel`], hand the
//!    `Pty` to the blocking pool via `PtyGuard::teardown`, then wait for the
//!    reader with [`SessionExitGate::join_reader_by`], bounded by
//!    [`READER_JOIN_TIMEOUT`]. On expiry the handle is dropped and the task is
//!    left running, so a wedged reader delays nothing beyond the bound.
//!
//! The join **must** hold no lock: a reader on its way out takes the
//! `live_sessions` and then the `workspace_manager` write guards inside the
//! exit finalizer, so joining under either one would stall for the whole
//! bound. It is also the last step of a close, so that stall can only ever
//! delay the exit notification — the session is already unwired from the
//! registry, the workspace, and the client's attached set by the time it runs.
//!
//! That guard pair is the server-wide lock order too — `live_sessions` before
//! `workspace_manager`, never the reverse, and neither held across an
//! `.await`. Paths that need a workspace read first release it before they
//! touch the registry, so the two never overlap in the opposite direction.
//!
//! Process exit is deliberately outside this protocol. A handoff must not
//! cancel readers on its way out: cancellation drives the funnel, and the
//! funnel would publish `SessionExited` for sessions that are being handed to
//! the incoming server alive.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// How long a close path waits for a cancelled reader before it detaches the
/// task and moves on (spec 017, Q7).
///
/// The reader normally observes cancellation on its next loop turn, so this
/// only bites when it is wedged somewhere the cancel cannot reach — a sink
/// whose participant stopped draining, or a `Term` lock held by a long
/// snapshot. Detaching there is strictly better than inheriting the stall:
/// the session is already unwired, and the task holds nothing the close needs.
pub const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome of a bounded reader join, for the caller to log.
#[derive(Debug)]
pub enum ReaderJoin {
    /// The reader task ended inside the bound.
    Joined,
    /// No handle to join: the reader already ended and an earlier close took
    /// it, or this close beat [`SessionExitGate::set_reader`] to the gate.
    Absent,
    /// The reader task panicked or was aborted.
    Failed(tokio::task::JoinError),
    /// The bound expired. The handle was dropped and the task left running.
    Detached,
    /// The join was requested from the reader task itself, which can never
    /// complete. Refused instead of parked; the handle stays on the gate.
    SelfJoin,
}

/// Shared exit state for one session, held by the `LiveSession` and by its PTY
/// reader task so close paths, the reader, and the child-exit watcher all
/// arbitrate through the same gate.
#[derive(Debug)]
pub struct SessionExitGate {
    /// Cancellation signal for the reader task. A `watch` channel rather than
    /// a one-shot: the reader re-awaits it on every loop turn, and a receiver
    /// created before the flag flips still observes the flip.
    cancel_tx: watch::Sender<bool>,
    /// Winner-takes-all gate for the exit funnel.
    finalized: AtomicBool,
    /// Set once a child-exit watcher owns this session's exit status. While
    /// set, the reader yields the funnel to it whenever the master stream
    /// ends: that only proves every slave fd closed, which a live child can do
    /// on its own, so the watcher — not the reader — holds the authoritative
    /// status. Handoff-inherited sessions never arm one and keep that path
    /// with `exit_code: None`.
    watcher_armed: AtomicBool,
    /// Raised when the reader loop ends, for whatever reason. The child-exit
    /// watcher waits on it so the PTY finishes draining before `SessionExited`
    /// goes out, keeping a dying session's last bytes ahead of its exit frame.
    reader_done: watch::Sender<bool>,
    /// The reader task's handle, retained so teardown can join it instead of
    /// detaching an unbounded task.
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl Default for SessionExitGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionExitGate {
    pub fn new() -> Self {
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        let (reader_done, _reader_done_rx) = watch::channel(false);
        Self {
            cancel_tx,
            finalized: AtomicBool::new(false),
            watcher_armed: AtomicBool::new(false),
            reader_done,
            reader: Mutex::new(None),
        }
    }

    /// Subscribe a reader task to this gate's cancellation signal.
    pub fn subscribe(&self) -> CancelWaiter {
        CancelWaiter { rx: self.cancel_tx.subscribe() }
    }

    /// Ask the reader task to stop. Idempotent, never blocks, and safe to call
    /// after the reader has already exited.
    pub fn cancel(&self) {
        self.cancel_tx.send_replace(true);
    }

    /// Whether cancellation has been raised on this session.
    pub fn is_cancelled(&self) -> bool {
        *self.cancel_tx.borrow()
    }

    /// Retain the reader task's handle so teardown can join it.
    ///
    /// A close that lands in the window between the registry insert and this
    /// call takes `None` and detaches instead; it has already cancelled the
    /// gate, so the reader still terminates on its own.
    pub fn set_reader(&self, handle: JoinHandle<()>) {
        *self.lock_reader() = Some(handle);
    }

    /// Take the reader's handle for a bounded join. Returns `None` when the
    /// handle was already taken or was never stored.
    pub fn take_reader(&self) -> Option<JoinHandle<()>> {
        self.lock_reader().take()
    }

    /// Wait for the reader task to end, giving up at `deadline`.
    ///
    /// Step 3 of the close protocol documented at the top of this module. The
    /// caller must already have raised [`SessionExitGate::cancel`] — this only
    /// waits — and must hold no lock, because the reader takes the
    /// `live_sessions` and `workspace_manager` write guards on its way out.
    ///
    /// `CloseWindow` passes one deadline to every session it is closing, so a
    /// window full of wedged readers still costs a single
    /// [`READER_JOIN_TIMEOUT`] rather than one per pane.
    pub async fn join_reader_by(&self, deadline: Instant) -> ReaderJoin {
        let handle = {
            let mut slot = self.lock_reader();
            // A task cannot join itself: the finalizer runs on the reader too,
            // and taking this route from there would park it until the bound
            // expired. Leave the handle in place for the close that is
            // actually driving the teardown.
            if slot.as_ref().is_some_and(|handle| tokio::task::try_id() == Some(handle.id())) {
                return ReaderJoin::SelfJoin;
            }
            slot.take()
        };
        let Some(handle) = handle else { return ReaderJoin::Absent };
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(Ok(())) => ReaderJoin::Joined,
            Ok(Err(err)) => ReaderJoin::Failed(err),
            // Dropping the handle detaches; aborting instead would cut the
            // reader mid-chunk and could strand the `Term` mutex.
            Err(_elapsed) => ReaderJoin::Detached,
        }
    }

    /// Declare that a child-exit watcher owns this session's exit status, so
    /// the reader's EOF path stops emitting on its behalf.
    pub fn arm_watcher(&self) {
        self.watcher_armed.store(true, Ordering::Release);
    }

    /// Whether a child-exit watcher is this session's authoritative emitter.
    pub fn has_watcher(&self) -> bool {
        self.watcher_armed.load(Ordering::Acquire)
    }

    /// Announce that the reader loop has ended. Idempotent.
    pub fn mark_reader_done(&self) {
        self.reader_done.send_replace(true);
    }

    /// Resolve once the reader loop has ended, immediately if it already had.
    ///
    /// The child-exit watcher awaits this (bounded by its own timeout) before
    /// it emits: the child's death and the reader's last read are two
    /// independent wakeups, and emitting `SessionExited` first would retire
    /// the pane on the client while its final output was still in flight.
    pub async fn reader_finished(&self) {
        let mut rx = self.reader_done.subscribe();
        // `Err` only occurs once every sender is gone, which cannot happen
        // while the caller holds the gate; treat it as "finished" either way
        // rather than parking the watcher forever.
        let _finished = rx.wait_for(|flag| *flag).await.is_ok();
    }

    /// Elect this caller to run the session's one-shot exit finalizer.
    /// Returns `true` exactly once per session — for whichever exit path
    /// arrives first — and `false` for every later caller.
    pub fn claim_exit(&self) -> bool {
        !self.finalized.swap(true, Ordering::AcqRel)
    }

    /// Whether some path has already claimed this session's exit.
    ///
    /// Read-only counterpart to [`SessionExitGate::claim_exit`], for callers
    /// that must not become the finalizer but would act wrongly on a session
    /// already on its way out. `true` is durable — the gate never reopens — so
    /// a reader that observes it needs no further synchronisation.
    pub fn is_finalized(&self) -> bool {
        self.finalized.load(Ordering::Acquire)
    }

    fn lock_reader(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        // The critical sections are a store and a take, neither of which can
        // panic, so poisoning cannot occur in practice; recover rather than
        // propagate if it somehow does.
        self.reader.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A reader task's view of its gate's cancellation signal. [`Self::cancelled`]
/// is cancel-safe, so it can sit in the reader's `select!` without losing the
/// wakeup when another branch wins the race.
#[derive(Debug)]
pub struct CancelWaiter {
    rx: watch::Receiver<bool>,
}

impl CancelWaiter {
    /// Resolve once the gate is cancelled, immediately if cancellation was
    /// already raised before this call.
    pub async fn cancelled(&mut self) {
        // `Err` only occurs once every sender is gone, which cannot happen
        // while the reader holds its own gate; resolve either way rather than
        // parking a doomed reader forever.
        let _cancelled = self.rx.wait_for(|flag| *flag).await.is_ok();
    }
}
