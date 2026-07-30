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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;

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
