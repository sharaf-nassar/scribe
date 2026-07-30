//! Child-process identity tokens (spec 017 US7-2).
//!
//! A PID on its own does not name a process. Once a child is reaped the kernel
//! is free to hand its number to something unrelated, so a `kill(pid, SIGHUP)`
//! issued from state captured earlier can hang up a stranger's program. This
//! matters for handoff-restored sessions in particular: their children were
//! reparented to init when the old server exited, so `init` reaps them the
//! moment they die and the server never learns the PID went stale.
//!
//! Pairing the PID with the process's start time closes that gap. The kernel
//! never rewrites a live process's start time, so an observed value that
//! differs from the recorded one proves the number was recycled. The pair is
//! only comparable within one boot of one host, which is exactly the handoff
//! case — the successor server inherits children from a process it replaced
//! in place.

// @lat: [[server#Handoff#Defuse Strategy]]

/// Per-boot identity token for a live PID.
///
/// Linux: the `starttime` field of `/proc/<pid>/stat`, in clock ticks since
/// boot. macOS: the process start timestamp in microseconds. The value is
/// opaque — only equality against another token read on the same boot means
/// anything.
pub type ChildIdentity = u64;

/// Result of matching a recorded [`ChildIdentity`] against the process that
/// currently holds the PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCheck {
    /// Recorded and observed tokens agree: the PID is still our child.
    Match,
    /// Nothing was recorded. Either the handoff sender predates this field or
    /// the platform cannot report a start time; the PID is unproven either
    /// way, so callers must not signal it.
    Unrecorded,
    /// The PID does not resolve to a process we can read. It exited, and
    /// signalling would either fail or reach whoever inherits the number next.
    Gone,
    /// The PID resolves to a process that is not the one we recorded.
    Recycled,
}

impl IdentityCheck {
    /// Whether a signal aimed at this PID is provably aimed at our child.
    pub fn may_signal(self) -> bool {
        matches!(self, Self::Match)
    }
}

/// Read the identity token for `pid`, or `None` when it cannot be determined
/// (process gone, unparsable procfs entry, or an unsupported platform).
pub fn read_child_identity(pid: u32) -> Option<ChildIdentity> {
    platform::read_start_time(pid)
}

/// Compare `recorded` against the process currently holding `pid`.
///
/// The observed token is read at call time, so this narrows the reuse window
/// to the gap between this read and the caller's signal rather than closing
/// it outright. Closing it outright would need a pidfd held since the child
/// was forked, and handoff-inherited sessions never have one —
/// [`crate::child_watch`] leaves their `child_pidfd` at `None` because a
/// different process spawned them. Everything the PID-reuse bug actually
/// looked like in practice — a child that died minutes ago and a number since
/// handed out — is rejected here.
pub fn check_child_identity(pid: u32, recorded: Option<ChildIdentity>) -> IdentityCheck {
    classify(recorded, read_child_identity(pid))
}

fn classify(recorded: Option<ChildIdentity>, observed: Option<ChildIdentity>) -> IdentityCheck {
    match (recorded, observed) {
        (None, _) => IdentityCheck::Unrecorded,
        (Some(_), None) => IdentityCheck::Gone,
        (Some(recorded), Some(observed)) => {
            if recorded == observed {
                IdentityCheck::Match
            } else {
                IdentityCheck::Recycled
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// `starttime` is field 22 of `/proc/<pid>/stat`, and the whitespace split
    /// used here starts at field 3 (`state`) because everything before it is
    /// consumed with `comm`.
    const STARTTIME_INDEX_AFTER_COMM: usize = 22 - 3;

    pub fn read_start_time(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_start_time(&stat)
    }

    /// Parse `starttime` out of a `/proc/<pid>/stat` line.
    ///
    /// `comm` is the raw executable name wrapped in parentheses and may itself
    /// contain spaces and parentheses, so the fixed-position fields only begin
    /// after its final `)`.
    pub(super) fn parse_start_time(stat: &str) -> Option<u64> {
        let (_, after_comm) = stat.rsplit_once(')')?;
        after_comm.split_ascii_whitespace().nth(STARTTIME_INDEX_AFTER_COMM)?.parse().ok()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    pub fn read_start_time(pid: u32) -> Option<u64> {
        let pid = i32::try_from(pid).ok()?;
        let (secs, micros) = crate::macos_proc::macos_proc_start_time(pid)?;
        Some(secs.saturating_mul(1_000_000).saturating_add(micros))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn read_start_time(_pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{IdentityCheck, check_child_identity, classify, read_child_identity};

    #[test]
    fn absent_recorded_identity_is_unrecorded() {
        assert_eq!(classify(None, Some(42)), IdentityCheck::Unrecorded);
        assert_eq!(classify(None, None), IdentityCheck::Unrecorded);
        assert!(!IdentityCheck::Unrecorded.may_signal());
    }

    #[test]
    fn vanished_process_is_gone() {
        assert_eq!(classify(Some(42), None), IdentityCheck::Gone);
        assert!(!IdentityCheck::Gone.may_signal());
    }

    #[test]
    fn differing_start_time_is_recycled() {
        assert_eq!(classify(Some(42), Some(43)), IdentityCheck::Recycled);
        assert!(!IdentityCheck::Recycled.may_signal());
    }

    #[test]
    fn equal_start_time_matches() {
        assert_eq!(classify(Some(42), Some(42)), IdentityCheck::Match);
        assert!(IdentityCheck::Match.may_signal());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn own_pid_reads_a_stable_identity() {
        let pid = std::process::id();
        let first = read_child_identity(pid);
        assert!(first.is_some(), "expected a start time for our own pid");
        assert_eq!(first, read_child_identity(pid));
        assert_eq!(check_child_identity(pid, first), IdentityCheck::Match);
    }

    /// A live PID carrying a recorded token from some earlier process is what
    /// PID reuse looks like from the server's side.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn stale_recorded_identity_on_a_live_pid_is_recycled() {
        let pid = std::process::id();
        let Some(actual) = read_child_identity(pid) else {
            return;
        };
        let stale = actual.wrapping_add(1);
        assert_eq!(check_child_identity(pid, Some(stale)), IdentityCheck::Recycled);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_pid_yields_no_identity() {
        // PID 0 is the kernel's "no process" sentinel and never has a
        // `/proc/0/stat`, so it stands in for a number nobody holds.
        assert_eq!(read_child_identity(0), None);
        assert_eq!(check_child_identity(0, Some(1)), IdentityCheck::Gone);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_comm_with_spaces_and_parens_is_skipped() {
        // `S` is field 3; every later field holds its own field number, so a
        // correct parse of field 22 (`starttime`) returns 22.
        let fields: Vec<String> = (4u64..=52).map(|n| n.to_string()).collect();
        let stat = format!("1234 (we (are) not) S {}\n", fields.join(" "));
        assert_eq!(super::platform::parse_start_time(&stat), Some(22));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn truncated_proc_stat_yields_no_identity() {
        assert_eq!(super::platform::parse_start_time("1234 (sh) S 1 2 3"), None);
        assert_eq!(super::platform::parse_start_time("garbage"), None);
    }
}
