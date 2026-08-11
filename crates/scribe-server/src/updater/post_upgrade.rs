//! What the surviving server owes after a zero-downtime upgrade.
//!
//! The process that performs a macOS update cannot report its own outcome: it
//! exits inside `install_update` when the handoff completes, so the terminal
//! `UpdateProgress` broadcast is never sent and its staging directory is never
//! dropped. A client is left showing "Downloading…" forever and ~20 MB leaks
//! under the runtime dir.
//!
//! Every established updater resolves this the same way — the process being
//! replaced never reports the outcome, the survivor does. Sparkle and
//! Squirrel.Mac relaunch the app and let the relaunched app be the signal;
//! nginx lets the new master own the report while the old master drains. This
//! module is that role for Scribe: the `--upgrade` server announces the
//! completion and reaps what its predecessor could not.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long after an upgrade a reconnecting client still gets the completion.
///
/// Clients reconnect a moment after the handoff, so the announcement cannot be
/// delivered synchronously. Bounded so a window opened much later is not told
/// about an upgrade it never witnessed.
pub const ANNOUNCE_WINDOW: Duration = Duration::from_mins(1);

/// A pending "the upgrade finished" message owed to reconnecting clients.
#[derive(Debug)]
pub struct UpgradeAnnouncement {
    version: String,
    expires_at: Instant,
}

impl UpgradeAnnouncement {
    /// Records that this process came up from an upgrade at `now`.
    #[must_use]
    pub fn new(version: String, now: Instant) -> Self {
        Self { version, expires_at: now + ANNOUNCE_WINDOW }
    }

    /// The version to announce to a client registering at `now`, if still owed.
    #[must_use]
    pub fn version_for(&self, now: Instant) -> Option<&str> {
        (now < self.expires_at).then_some(self.version.as_str())
    }
}

/// Set once, by an `--upgrade` process, at startup.
static ANNOUNCEMENT: OnceLock<UpgradeAnnouncement> = OnceLock::new();

/// Records that this process came up from an upgrade, so the first clients to
/// reconnect are told the update finished.
pub fn record_upgrade(version: &str) {
    drop(ANNOUNCEMENT.set(UpgradeAnnouncement::new(version.to_owned(), Instant::now())));
}

/// The version still owed to a reconnecting client, if any.
#[must_use]
pub fn pending_version() -> Option<String> {
    ANNOUNCEMENT.get()?.version_for(Instant::now()).map(str::to_owned)
}

/// Staging directories left behind by a predecessor that was replaced mid-install.
///
/// Any `update-*` directory present at startup is orphaned by definition: an
/// update is driven by a live server, and this process has not started one.
#[must_use]
pub fn orphaned_stage_dirs(runtime_entries: &[PathBuf]) -> Vec<PathBuf> {
    runtime_entries
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("update-"))
        })
        .cloned()
        .collect()
}

/// Removes the directories [`orphaned_stage_dirs`] identifies. Best-effort:
/// a failure here must never keep the server from starting.
pub fn reap_orphaned_stages(runtime_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    let paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();

    for stage in orphaned_stage_dirs(&paths) {
        match std::fs::remove_dir_all(&stage) {
            Ok(()) => tracing::info!(path = %stage.display(), "reaped orphaned update staging dir"),
            Err(e) => {
                tracing::warn!(path = %stage.display(), "could not reap update staging dir: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#Post-upgrade announcement#Announces to a client reconnecting after the handoff]]
    #[test]
    fn announces_to_a_client_reconnecting_after_the_handoff() {
        let start = Instant::now();
        let announcement = UpgradeAnnouncement::new("0.1.5".to_owned(), start);

        assert_eq!(
            announcement.version_for(start + Duration::from_secs(2)),
            Some("0.1.5"),
            "a client reconnecting moments after the handoff is owed the completion"
        );
    }

    // @lat: [[test#Test Harness#Post-upgrade announcement#Stops announcing once the window closes]]
    #[test]
    fn stops_announcing_once_the_window_closes() {
        let start = Instant::now();
        let announcement = UpgradeAnnouncement::new("0.1.5".to_owned(), start);

        assert_eq!(
            announcement.version_for(start + ANNOUNCE_WINDOW + Duration::from_secs(1)),
            None,
            "a window opened much later never witnessed the upgrade"
        );
    }

    // @lat: [[test#Test Harness#Post-upgrade announcement#Orphaned staging dirs are identified by prefix]]
    #[test]
    fn orphaned_staging_dirs_are_identified_by_prefix() {
        let entries = vec![
            PathBuf::from("/run/scribe/update-6bcce7cd"),
            PathBuf::from("/run/scribe/server.sock"),
            PathBuf::from("/run/scribe/handoff.sock"),
            PathBuf::from("/run/scribe/update-0de19769"),
            PathBuf::from("/run/scribe/server.lock"),
        ];

        let orphans = orphaned_stage_dirs(&entries);

        assert_eq!(
            orphans,
            vec![
                PathBuf::from("/run/scribe/update-6bcce7cd"),
                PathBuf::from("/run/scribe/update-0de19769"),
            ],
            "only staging dirs are reaped — sockets and lock files must survive"
        );
    }

    // @lat: [[test#Test Harness#Post-upgrade announcement#Sockets are never reaped]]
    #[test]
    fn sockets_are_never_reaped() {
        let entries = vec![
            PathBuf::from("/run/scribe/server.sock"),
            PathBuf::from("/run/scribe/handoff.sock"),
            PathBuf::from("/run/scribe/settings.sock"),
        ];

        assert!(
            orphaned_stage_dirs(&entries).is_empty(),
            "reaping a live socket would strand every connected client"
        );
    }
}
