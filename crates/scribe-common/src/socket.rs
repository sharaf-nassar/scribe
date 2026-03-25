use std::path::PathBuf;

use nix::unistd::geteuid;

/// Returns the platform-specific runtime directory for scribe sockets.
///
/// - Linux: `/run/user/{uid}/scribe/`
/// - macOS: `$TMPDIR/scribe-{uid}/` (falls back to `~/Library/Application Support/Scribe/run/`)
fn runtime_dir() -> PathBuf {
    platform_runtime_dir(geteuid().as_raw())
}

/// Linux: use the standard XDG runtime directory.
#[cfg(target_os = "linux")]
fn platform_runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/scribe"))
}

/// macOS: use `$TMPDIR`-based directory (per-user, fast, no spaces).
/// Falls back to `~/Library/Application Support/Scribe/run/`.
#[cfg(target_os = "macos")]
fn platform_runtime_dir(uid: u32) -> PathBuf {
    std::env::var("TMPDIR").map_or_else(
        |_| {
            dirs::home_dir().map_or_else(
                || PathBuf::from(format!("/tmp/scribe-{uid}")),
                |home| home.join("Library/Application Support/Scribe/run"),
            )
        },
        |tmpdir| {
            let mut p = PathBuf::from(tmpdir);
            p.push(format!("scribe-{uid}"));
            p
        },
    )
}

/// Catch-all for other Unix platforms — same pattern as macOS.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_runtime_dir(uid: u32) -> PathBuf {
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let mut p = PathBuf::from(tmpdir);
        p.push(format!("scribe-{uid}"));
        p
    } else {
        PathBuf::from(format!("/tmp/scribe-{uid}"))
    }
}

/// Returns the platform-specific socket path for the scribe server.
///
/// - Linux: `/run/user/{uid}/scribe/server.sock`
/// - macOS: `$TMPDIR/scribe-{uid}/server.sock`
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
/// - macOS: `$TMPDIR/scribe-{uid}/settings.sock`
#[must_use]
pub fn settings_socket_path() -> PathBuf {
    runtime_dir().join("settings.sock")
}

/// Returns the lock file path for the settings singleton.
///
/// - Linux: `/run/user/{uid}/scribe/settings.lock`
/// - macOS: `$TMPDIR/scribe-{uid}/settings.lock`
#[must_use]
pub fn settings_lock_path() -> PathBuf {
    runtime_dir().join("settings.lock")
}

/// Returns the lock file path for the server singleton.
///
/// - Linux: `/run/user/{uid}/scribe/server.lock`
/// - macOS: `$TMPDIR/scribe-{uid}/server.lock`
#[must_use]
pub fn server_lock_path() -> PathBuf {
    runtime_dir().join("server.lock")
}

/// Returns the handoff socket path for zero-downtime upgrades.
///
/// - Linux: `/run/user/{uid}/scribe/handoff.sock`
/// - macOS: `$TMPDIR/scribe-{uid}/handoff.sock`
#[must_use]
pub fn handoff_socket_path() -> PathBuf {
    runtime_dir().join("handoff.sock")
}
