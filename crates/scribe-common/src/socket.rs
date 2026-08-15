use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use nix::unistd::geteuid;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::{AppIdentity, current_identity};

/// Filename prefix reserved for private restore-child focus endpoints.
pub const CLIENT_FOCUS_SOCKET_PREFIX: &str = "client-focus-";

/// Random process generation carried by restore-child focus transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientFocusGeneration(Uuid);

impl ClientFocusGeneration {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Short random tag used only in the Unix socket filename.
    #[must_use]
    pub fn socket_tag(self) -> String {
        self.0.simple().to_string()[..16].to_owned()
    }

    #[must_use]
    pub fn socket_name(self) -> String {
        format!("{CLIENT_FOCUS_SOCKET_PREFIX}{}.sock", self.socket_tag())
    }
}

impl Default for ClientFocusGeneration {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ClientFocusGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ClientFocusGeneration {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

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

/// Returns the socket path for the terminal-client singleton.
#[must_use]
pub fn client_socket_path() -> PathBuf {
    runtime_dir().join("client.sock")
}

/// Returns the private focus endpoint path for one restore-child generation.
#[must_use]
pub fn client_focus_socket_path(generation: ClientFocusGeneration) -> PathBuf {
    client_focus_socket_path_for(current_identity(), geteuid().as_raw(), generation)
}

fn client_focus_socket_path_for(
    identity: AppIdentity,
    uid: u32,
    generation: ClientFocusGeneration,
) -> PathBuf {
    platform_runtime_dir(identity, uid).join(generation.socket_name())
}

/// Returns the lock file path for terminal-client singleton acquisition.
#[must_use]
pub fn client_lock_path() -> PathBuf {
    runtime_dir().join("client.lock")
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

#[cfg(test)]
mod shared_runtime_tests {
    use super::{
        ClientFocusGeneration, client_focus_socket_path_for, client_lock_path, client_socket_path,
        server_socket_path,
    };
    use crate::app::AppIdentity;

    // @lat: [[test#Test Harness#Terminal Client Singleton#Client paths stay flavor scoped]]
    #[test]
    fn terminal_client_paths_share_the_flavor_runtime_directory() {
        let server_socket = server_socket_path();
        let runtime_dir = server_socket.parent().expect("server socket has a parent");

        assert_eq!(client_socket_path().parent(), Some(runtime_dir));
        assert_eq!(client_lock_path().parent(), Some(runtime_dir));
        assert_eq!(client_socket_path().file_name().unwrap(), "client.sock");
        assert_eq!(client_lock_path().file_name().unwrap(), "client.lock");
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Focus endpoints stay short and flavor scoped]]
    #[test]
    fn focus_endpoint_paths_stay_short_and_flavor_scoped() {
        let generation: ClientFocusGeneration =
            "12345678-90ab-cdef-1234-567890abcdef".parse().unwrap();
        let stable = client_focus_socket_path_for(AppIdentity::stable(), 1000, generation);
        let dev = client_focus_socket_path_for(AppIdentity::dev(), 1000, generation);

        assert_ne!(stable.parent(), dev.parent());
        assert_eq!(stable.file_name(), dev.file_name());
        assert_eq!(stable.file_name().unwrap(), "client-focus-1234567890abcdef.sock");
        assert!(stable.file_name().unwrap().to_string_lossy().len() < 40);
    }
}
