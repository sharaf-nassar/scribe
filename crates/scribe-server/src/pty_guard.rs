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
//!
//! Moving that wait off the worker does not bound it. The `waitpid` still never
//! returns while a child that swallowed the `SIGHUP` lives, and the runtime's
//! shutdown waits on the blocking pool, so process exit inherited the same
//! unbounded wait and hung indefinitely behind one closed session. Every
//! teardown therefore arms a watchdog that escalates to `SIGKILL` once
//! [`TEARDOWN_KILL_GRACE`] has passed with no reap. The watchdog runs on a plain
//! OS thread rather than the blocking pool because the moment it matters most —
//! runtime shutdown, where a queued blocking task is dropped instead of run and
//! the `Pty` is dropped inline on the thread doing the shutting down — is
//! exactly the moment the runtime can no longer schedule anything.

use std::mem::ManuallyDrop;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use alacritty_terminal::tty::Pty;
use rustix::process::Signal;
use tokio::runtime::Handle;
use tracing::warn;

/// How long a torn-down child has to act on its `SIGHUP` before the teardown
/// escalates to `SIGKILL`.
///
/// The same bound as the reader join in [`crate::session_exit`], for the same
/// reason: past it, whatever the close is waiting on has had every chance to
/// finish and waiting longer only propagates the stall.
pub const TEARDOWN_KILL_GRACE: Duration = Duration::from_secs(2);

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

    /// Send the child `SIGHUP` and reap it on the blocking pool, escalating to
    /// `SIGKILL` after [`TEARDOWN_KILL_GRACE`].
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

/// Run `Pty::drop` (SIGHUP + blocking `waitpid`) on the blocking pool, under a
/// watchdog that escalates to `SIGKILL` if the child outlives the grace.
///
/// Outside a runtime — tests, and the tail of process shutdown — there is no
/// pool to move onto, so the drop runs inline; nothing is awaiting the caller
/// there anyway, and the watchdog bounds it just the same.
fn drop_off_worker(pty: Pty) {
    let reap = Arc::new(ReapSignal::default());
    spawn_kill_escalation(ChildKill::open(pty.child().id()), Arc::clone(&reap));
    let reap_child = move || {
        drop(pty);
        reap.mark_reaped();
    };
    match Handle::try_current() {
        Ok(handle) => {
            // Detached on purpose: the whole point is that no close path waits
            // on the child. A never-scheduled task (runtime already shutting
            // down) drops the `Pty` on the shutdown thread instead, which is
            // the same work the old code did inline — and which the watchdog
            // armed above bounds either way.
            drop(handle.spawn_blocking(reap_child));
        }
        Err(_) => reap_child(),
    }
}

/// Arm the watchdog for one `Pty::drop`.
///
/// A plain OS thread, not a blocking-pool task: it has to fire while the
/// runtime is shutting down, where a queued task is dropped rather than run,
/// and the reap it unblocks is what that shutdown is waiting on. Detached — it
/// either observes the reap and returns, or kills and returns.
fn spawn_kill_escalation(child: ChildKill, reap: Arc<ReapSignal>) {
    let watchdog = std::thread::Builder::new().name("pty-teardown".to_owned()).spawn(move || {
        if reap.wait_reaped(TEARDOWN_KILL_GRACE) {
            return;
        }
        warn!(child_pid = child.child_pid, "PTY child outlived its SIGHUP — escalating to SIGKILL");
        child.kill();
    });
    if let Err(err) = watchdog {
        warn!(%err, "could not arm the PTY teardown watchdog — an unresponsive child parks its reap");
    }
}

/// Completion signal for one `Pty::drop`, shared with its watchdog.
#[derive(Default)]
struct ReapSignal {
    reaped: Mutex<bool>,
    reaped_cv: Condvar,
}

impl ReapSignal {
    /// Announce that `Pty::drop` returned: the child was signalled and reaped.
    fn mark_reaped(&self) {
        // The critical sections are a store and a load, neither of which can
        // panic, so poisoning cannot occur in practice; recover rather than
        // propagate if it somehow does.
        *self.reaped.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.reaped_cv.notify_all();
    }

    /// Wait up to `grace` for the reap, reporting whether it arrived.
    fn wait_reaped(&self, grace: Duration) -> bool {
        let pending = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        let (reaped, _timed_out) = self
            .reaped_cv
            .wait_timeout_while(pending, grace, |done| !*done)
            .unwrap_or_else(PoisonError::into_inner);
        *reaped
    }
}

/// Where the escalation aims its `SIGKILL`.
struct ChildKill {
    child_pid: u32,
    /// Opened before the reap so the escalation cannot land on a recycled PID:
    /// a `pidfd` names the process itself and goes stale — rather than
    /// re-pointing — once `waitpid` collects the child. `None` where there is
    /// no `pidfd` to open, leaving the PID as the only handle.
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
}

impl ChildKill {
    #[cfg(target_os = "linux")]
    fn open(child_pid: u32) -> Self {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};

        // Blocking: this fd is only ever signalled through, never polled.
        let pidfd = Pid::from_raw(child_pid.cast_signed())
            .and_then(|pid| pidfd_open(pid, PidfdFlags::empty()).ok());
        Self { child_pid, pidfd }
    }

    #[cfg(not(target_os = "linux"))]
    fn open(child_pid: u32) -> Self {
        Self { child_pid }
    }

    /// Kill the child, logging rather than propagating: the only caller is a
    /// watchdog with nothing left to try. `ESRCH` here means the reap won the
    /// race with the grace period, which is the outcome the kill wanted.
    fn kill(&self) {
        if let Err(err) = self.send_kill() {
            warn!(child_pid = self.child_pid, %err, "SIGKILL to an unreaped PTY child failed");
        }
    }

    #[cfg(target_os = "linux")]
    fn send_kill(&self) -> rustix::io::Result<()> {
        self.pidfd.as_ref().map_or_else(
            || kill_pid(self.child_pid),
            |pidfd| rustix::process::pidfd_send_signal(pidfd, Signal::KILL),
        )
    }

    #[cfg(not(target_os = "linux"))]
    fn send_kill(&self) -> rustix::io::Result<()> {
        kill_pid(self.child_pid)
    }
}

/// `kill(pid, SIGKILL)`, for platforms and failure paths with no `pidfd`.
fn kill_pid(child_pid: u32) -> rustix::io::Result<()> {
    use rustix::process::{Pid, kill_process};

    Pid::from_raw(child_pid.cast_signed())
        .map_or(Err(rustix::io::Errno::SRCH), |pid| kill_process(pid, Signal::KILL))
}
