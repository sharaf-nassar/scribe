//! Child-exit watcher (spec 017 US1-2).
//!
//! Exit detection used to ride entirely on PTY master EOF, which reports
//! nothing about *how* the child died — `SessionExited` always carried
//! `exit_code: None`. A master EOF is also the wrong signal: it only proves
//! every slave fd closed, which a live child can do on its own.
//!
//! This module watches the child directly. A `pidfd` opened at spawn becomes
//! readable when the child exits; the watcher then *peeks* the wait status
//! with `waitid(..., WNOWAIT)`, which leaves the child a zombie. That matters
//! for ownership: `alacritty_terminal`'s `Pty::drop` — routed off-worker by
//! [`crate::pty_guard::PtyGuard`] — still owns the reap. Peeking rather than
//! reaping means the status is never stolen from that `waitpid` and the PID
//! cannot be recycled underneath a later `kill`.
//!
//! Handoff-inherited sessions have no pidfd (the child was spawned by the
//! previous server process and reparented when it exited), so they arm no
//! watcher and keep the EOF path with `exit_code: None`. The same holds on
//! platforms without `pidfd`.

use std::os::fd::OwnedFd;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tracing::warn;

/// How a session's child process terminated.
///
/// A normal exit sets `exit_code`; a signal termination sets `signal`. The two
/// are separate fields rather than one overloaded number so a `SIGKILL` is
/// never mistaken for an exit status.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChildExit {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

impl ChildExit {
    /// No wait status was observed. Every exit path except the watcher —
    /// reader EOF, an explicit close, and any handoff-inherited session —
    /// reports this.
    pub const UNKNOWN: Self = Self { exit_code: None, signal: None };
}

/// Open a `pidfd` for a freshly spawned child.
///
/// Returns `None` when the platform has no `pidfd` or the open fails; the
/// session then falls back to EOF-based exit detection, exactly as
/// handoff-inherited sessions do.
#[cfg(target_os = "linux")]
#[must_use]
pub fn open_child_pidfd(child_pid: u32) -> Option<OwnedFd> {
    use rustix::process::{Pid, PidfdFlags, pidfd_open};

    let pid = Pid::from_raw(child_pid.cast_signed())?;
    // `NONBLOCK` so a spurious readability wakeup makes `waitid` return
    // `EAGAIN` instead of parking the watcher task on a live child.
    match pidfd_open(pid, PidfdFlags::NONBLOCK) {
        Ok(fd) => Some(fd),
        Err(err) => {
            warn!(child_pid, %err, "pidfd_open failed — falling back to EOF exit detection");
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn open_child_pidfd(_child_pid: u32) -> Option<OwnedFd> {
    None
}

/// A session's armed child-exit watcher: the `pidfd` registered with the
/// reactor, waiting for the child to die.
pub struct ChildExitWatcher {
    pidfd: AsyncFd<OwnedFd>,
    child_pid: u32,
}

impl ChildExitWatcher {
    /// Register `pidfd` with the current reactor.
    ///
    /// Returns `None` if registration fails, which leaves the session on the
    /// EOF path rather than arming a watcher that can never fire. Must be
    /// called from inside a tokio runtime.
    #[must_use]
    pub fn arm(pidfd: OwnedFd, child_pid: u32) -> Option<Self> {
        match AsyncFd::with_interest(pidfd, Interest::READABLE) {
            Ok(pidfd) => Some(Self { pidfd, child_pid }),
            Err(err) => {
                warn!(child_pid, %err, "pidfd reactor registration failed — no child-exit watcher");
                None
            }
        }
    }

    /// Resolve once the child has exited, with its wait status.
    ///
    /// The child is left in its zombie state so `Pty::drop`'s `waitpid` still
    /// finds it. A status that cannot be read degrades to [`ChildExit::UNKNOWN`]
    /// rather than stranding the session, since the caller is the only path
    /// left that can finalize it.
    pub async fn exited(self) -> ChildExit {
        if let Err(err) = self.pidfd.readable().await {
            warn!(
                child_pid = self.child_pid,
                %err,
                "pidfd readability failed — reporting an unknown exit status"
            );
            return ChildExit::UNKNOWN;
        }
        peek_child_exit(&self.pidfd, self.child_pid)
    }
}

/// Read the child's wait status without reaping it (`WNOWAIT`).
#[cfg(target_os = "linux")]
fn peek_child_exit(pidfd: &AsyncFd<OwnedFd>, child_pid: u32) -> ChildExit {
    use std::os::fd::AsFd as _;

    use rustix::process::{WaitId, WaitIdOptions, waitid};
    // Scoped to this Linux-only body: the macOS variant below never logs at
    // debug, so a module-level import would be unused off Linux.
    use tracing::debug;

    match waitid(
        WaitId::PidFd(pidfd.get_ref().as_fd()),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
    ) {
        Ok(Some(status)) => {
            let exit =
                ChildExit { exit_code: status.exit_status(), signal: status.terminating_signal() };
            debug!(child_pid, ?exit, "child-exit watcher peeked wait status");
            exit
        }
        // The child is still waitable-but-unchanged, or was already reaped by
        // a `Pty::drop` that beat us here. Neither can be recovered from.
        Ok(None) => {
            debug!(child_pid, "waitid reported no state change — unknown exit status");
            ChildExit::UNKNOWN
        }
        Err(err) => {
            warn!(child_pid, %err, "waitid failed — reporting an unknown exit status");
            ChildExit::UNKNOWN
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn peek_child_exit(_pidfd: &AsyncFd<OwnedFd>, _child_pid: u32) -> ChildExit {
    ChildExit::UNKNOWN
}
