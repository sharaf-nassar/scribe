//! launchd slot selection for a warm macOS server replacement.

use std::path::{Path, PathBuf};

use scribe_common::app::AppIdentity;
use scribe_common::macos_launchd::{self, LaunchdSlot, TrackedClient};

/// The newly installed client executable that owns `SMAppService` registration.
#[must_use]
pub fn client_executable(app_bundle: &Path, binary_name: &str) -> PathBuf {
    app_bundle.join("Contents/MacOS").join(binary_name)
}

/// Bundled definitions whose content is part of the server launch contract.
#[must_use]
pub fn launch_agent_paths(app_bundle: &Path, identity: AppIdentity) -> [PathBuf; 2] {
    let directory = app_bundle.join("Contents/Library/LaunchAgents");
    [
        directory.join(macos_launchd::plist_name(identity, LaunchdSlot::Primary)),
        directory.join(macos_launchd::plist_name(identity, LaunchdSlot::Alternate)),
    ]
}

/// Slot occupied by the running updater process.
#[must_use]
pub fn current_slot(args: impl IntoIterator<Item = impl AsRef<str>>) -> LaunchdSlot {
    LaunchdSlot::from_args(args).unwrap_or(LaunchdSlot::Primary)
}

/// Inactive slot that launchd can start alongside the current process.
#[must_use]
pub const fn replacement_slot(current: LaunchdSlot) -> LaunchdSlot {
    current.other()
}

/// Argument that tells the replacement bundle which slot is currently active.
#[must_use]
pub fn registration_argument(active_slot: LaunchdSlot) -> String {
    active_slot.registration_argument()
}

/// Relay argument carrying the old server PID and exact pre-install client PIDs.
#[must_use]
pub fn client_relaunch_argument(old_server_pid: Option<u32>, clients: &[TrackedClient]) -> String {
    let server = old_server_pid.map_or_else(|| String::from("unchanged"), |pid| pid.to_string());
    let clients = clients
        .iter()
        .map(|client| format!("{}@{}", client.pid, client.start_time_secs))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}{server}:{clients}", macos_launchd::RELAUNCH_CLIENTS_ARG_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#Test Harness#macOS updater handoff command#Uses the replacement bundle registrar]]
    #[test]
    fn uses_the_replacement_bundle_registrar() {
        assert_eq!(
            client_executable(Path::new("/Applications/Scribe.app"), "scribe-client"),
            PathBuf::from("/Applications/Scribe.app/Contents/MacOS/scribe-client")
        );
    }

    // @lat: [[test#Test Harness#macOS updater handoff command#Alternates launchd slots]]
    #[test]
    fn alternates_launchd_slots() {
        assert_eq!(current_slot(["scribe-server"]), LaunchdSlot::Primary);
        assert_eq!(
            current_slot(["scribe-server", "--upgrade", "--launchd-slot=alternate"]),
            LaunchdSlot::Alternate
        );
        assert_eq!(replacement_slot(LaunchdSlot::Alternate), LaunchdSlot::Primary);
        assert_eq!(
            registration_argument(LaunchdSlot::Primary),
            "--register-launchd-replacement-for=primary"
        );
        assert_eq!(
            launch_agent_paths(Path::new("/Applications/Scribe.app"), AppIdentity::stable()),
            [
                PathBuf::from(
                    "/Applications/Scribe.app/Contents/Library/LaunchAgents/com.scribe.server.plist"
                ),
                PathBuf::from(
                    "/Applications/Scribe.app/Contents/Library/LaunchAgents/com.scribe.server.alternate.plist"
                ),
            ]
        );
        assert_eq!(
            client_relaunch_argument(
                Some(41),
                &[
                    TrackedClient { pid: 7, start_time_secs: 70 },
                    TrackedClient { pid: 9, start_time_secs: 90 },
                ]
            ),
            "--relaunch-clients-after-server=41:7@70,9@90"
        );
        assert_eq!(
            client_relaunch_argument(None, &[TrackedClient { pid: 7, start_time_secs: 70 }]),
            "--relaunch-clients-after-server=unchanged:7@70"
        );
    }
}
