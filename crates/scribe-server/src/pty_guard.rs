//! Off-worker PTY teardown (spec 017 US1-1).
//!
//! `alacritty_terminal::tty::Pty::drop` sends `SIGHUP` to the child and then
//! does a blocking `waitpid`. Every close path used to run that drop on a
//! Tokio worker — `CloseWindow` did it while holding the global
//! `live_sessions` write guard — so a child that ignores `SIGHUP`
//! (`trap '' HUP; sleep inf`) parked a worker thread, and with it the
//! registry, for as long as the child stayed alive.
//!
//! [`PtyGuard`] wraps the pinned `alacritty_terminal` `Pty` rather than
//! forking it. [`PtyGuard::teardown`] moves the inner `Pty` onto the blocking
//! pool, so the signal and the wait happen off every worker and outside every
//! lock; [`PtyGuard::defuse`] keeps the handoff path's `ManuallyDrop` leak, so
//! the old server hands its children to the new one without hanging them up.
//! Dropping a guard that reached neither call is not a correctness hazard —
//! `Drop` takes the same off-worker route as `teardown`.

use std::mem::ManuallyDrop;

use alacritty_terminal::tty::Pty;
use tokio::runtime::Handle;

/// Owns a session's `Pty` and guarantees its blocking `Drop` never runs on a
/// Tokio worker.
///
/// `None` inside means the guard was already torn down or defused; the value
/// is kept so both consuming methods and `Drop` can share one teardown route.
pub struct PtyGuard {
    inner: Option<Pty>,
}

impl PtyGuard {
    pub fn new(pty: Pty) -> Self {
        Self { inner: Some(pty) }
    }

    /// Send the child `SIGHUP` and reap it on the blocking pool.
    ///
    /// Returns immediately: the caller never waits on the child, so this is
    /// safe to call from an async task and — unlike the drop it replaces —
    /// cannot stall a close path behind a child that ignores `SIGHUP`.
    pub fn teardown(mut self) {
        if let Some(pty) = self.inner.take() {
            drop_off_worker(pty);
        }
    }

    /// Leak the `Pty` so its `Drop` never runs (hot-reload handoff).
    ///
    /// The new server already holds the master fd via `SCM_RIGHTS`, so the
    /// outgoing process must not signal the child on its way out.
    pub fn defuse(mut self) {
        if let Some(pty) = self.inner.take() {
            let _defused = ManuallyDrop::new(pty);
        }
    }
}

impl Drop for PtyGuard {
    fn drop(&mut self) {
        if let Some(pty) = self.inner.take() {
            drop_off_worker(pty);
        }
    }
}

/// Run `Pty::drop` (SIGHUP + blocking `waitpid`) on the blocking pool.
///
/// Outside a runtime — tests, and the tail of process shutdown — there is no
/// pool to move onto, so the drop runs inline; nothing is awaiting the caller
/// there anyway.
fn drop_off_worker(pty: Pty) {
    match Handle::try_current() {
        Ok(handle) => {
            // Detached on purpose: the whole point is that no close path waits
            // on the child. A never-scheduled task (runtime already shutting
            // down) drops the `Pty` on the shutdown thread instead, which is
            // the same work the old code did inline.
            drop(handle.spawn_blocking(move || drop(pty)));
        }
        Err(_) => drop(pty),
    }
}
