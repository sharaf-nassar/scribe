use std::path::PathBuf;

use nix::unistd::geteuid;

use crate::app::{AppIdentity, current_identity};

/// Returns the platform-specific runtime directory for scribe sockets.
///
/// - Linux: `/run/user/{uid}/scribe/`
/// - macOS: `~/Library/Application Support/Scribe/run/`
fn runtime_dir() -> PathBuf {
    platform_runtime_dir(current_identity(), geteuid().as_raw())
}

/// Linux: use the standard XDG runtime directory.
#[cfg(target_os = "linux")]
fn platform_runtime_dir(identity: AppIdentity, uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/{}", identity.runtime_dir_name()))
}

/// macOS: use a stable per-user Application Support directory so GUI apps and
/// launchd agents agree on the same socket path.
#[cfg(target_os = "macos")]
fn platform_runtime_dir(identity: AppIdentity, uid: u32) -> PathBuf {
    dirs::home_dir().map_or_else(
        || PathBuf::from(format!("/tmp/{}-{uid}", identity.runtime_dir_name())),
        |home| macos_runtime_dir_for_home(identity, &home),
    )
}

#[cfg(target_os = "macos")]
fn macos_runtime_dir_for_home(identity: AppIdentity, home: &std::path::Path) -> PathBuf {
    identity.macos_support_dir(home).join("run")
}

/// Catch-all for other Unix platforms — same pattern as macOS.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_runtime_dir(identity: AppIdentity, uid: u32) -> PathBuf {
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let mut p = PathBuf::from(tmpdir);
        p.push(format!("{}-{uid}", identity.runtime_dir_name()));
        p
    } else {
        PathBuf::from(format!("/tmp/{}-{uid}", identity.runtime_dir_name()))
    }
}

/// Returns the platform-specific socket path for the scribe server.
///
/// - Linux: `/run/user/{uid}/scribe/server.sock`
/// - macOS: `~/Library/Application Support/Scribe/run/server.sock`
#[must_use]
pub fn server_socket_path() -> PathBuf {
    runtime_dir().join("server.sock")
}

/// Returns the current process's effective UID as a raw `u32`.
#[must_use]
pub fn current_uid() -> u32 {
    geteuid().as_raw()
}

/// Returns the socket path for the settings singleton.
///
/// - Linux: `/run/user/{uid}/scribe/settings.sock`
/// - macOS: `~/Library/Application Support/Scribe/run/settings.sock`
#[must_use]
pub fn settings_socket_path() -> PathBuf {
    runtime_dir().join("settings.sock")
}

/// Returns the lock file path for the settings singleton.
///
/// - Linux: `/run/user/{uid}/scribe/settings.lock`
/// - macOS: `~/Library/Application Support/Scribe/run/settings.lock`
#[must_use]
pub fn settings_lock_path() -> PathBuf {
    runtime_dir().join("settings.lock")
}

/// Returns the lock file path for the server singleton.
///
/// - Linux: `/run/user/{uid}/scribe/server.lock`
/// - macOS: `~/Library/Application Support/Scribe/run/server.lock`
#[must_use]
pub fn server_lock_path() -> PathBuf {
    runtime_dir().join("server.lock")
}

/// Returns the handoff socket path for zero-downtime upgrades.
///
/// - Linux: `/run/user/{uid}/scribe/handoff.sock`
/// - macOS: `~/Library/Application Support/Scribe/run/handoff.sock`
#[must_use]
pub fn handoff_socket_path() -> PathBuf {
    runtime_dir().join("handoff.sock")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{AppIdentity, macos_runtime_dir_for_home};

    #[test]
    fn macos_runtime_dir_uses_application_support_for_stable() {
        let dir = macos_runtime_dir_for_home(AppIdentity::stable(), Path::new("/Users/tester"));
        assert_eq!(dir, PathBuf::from("/Users/tester/Library/Application Support/Scribe/run"));
    }

    #[test]
    fn macos_runtime_dir_uses_application_support_for_dev() {
        let dir = macos_runtime_dir_for_home(AppIdentity::dev(), Path::new("/Users/tester"));
        assert_eq!(dir, PathBuf::from("/Users/tester/Library/Application Support/Scribe Dev/run"));
    }
}
