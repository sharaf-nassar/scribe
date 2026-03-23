use std::path::PathBuf;

use nix::unistd::geteuid;

/// Returns the platform-specific socket path for the scribe server.
///
/// On Linux: `/run/user/{uid}/scribe/server.sock`
#[must_use]
pub fn server_socket_path() -> PathBuf {
    let uid = geteuid();
    PathBuf::from(format!("/run/user/{uid}/scribe/server.sock"))
}

/// Returns the current process's effective UID as a raw `u32`.
#[must_use]
pub fn current_uid() -> u32 {
    geteuid().as_raw()
}
