//! Local `scribe-server` lifecycle management for the GPUI client.
//!
//! The client owns starting, refreshing, and cold-restarting the per-user
//! server it talks to over the local IPC socket. Ported from the legacy
//! client's `ipc_client.rs` server-lifecycle helpers:
//!
//! - **Auto-start:** [`connect_or_start_server`] connects to the socket, or
//!   starts the systemd user service and waits for the socket to appear.
//! - **Stale-server refresh:** [`stale_server_reason`] is the pure decision
//!   that flags a connected server whose on-disk binary drifted (different
//!   path, or rebuilt after the running process started) so the caller can
//!   request a refresh.
//! - **Cold-restart recovery:** [`perform_linux_cold_restart`] force-stops the
//!   systemd unit and any surviving processes, clears stale socket files, and
//!   starts a fresh server — the last-ditch path when a wedged server holds the
//!   lock but its accept loop is dead.
//!
//! The IPC protocol and server binary are frozen; this module only spawns and
//! signals the existing server. macOS launchd support from the old client is
//! deferred with the rest of the macOS port (see plan Phase H) and is not
//! recreated here.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use scribe_common::app::current_identity;
use scribe_common::socket::{handoff_socket_path, server_socket_path};

/// Maximum time to wait for a freshly started server to accept connections.
pub const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the server socket to become connectable.
pub const SERVER_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Snapshot of a connected server process, used by [`stale_server_reason`].
#[derive(Debug, Clone)]
pub struct ConnectedServerInfo {
    /// Peer PID of the connected server.
    pub pid: i32,
    /// Resolved executable path of the running server, if known.
    pub exe_path: Option<PathBuf>,
    /// Process start time in seconds since the Unix epoch, if known.
    pub start_time_secs: Option<u64>,
}

/// Canonicalize both paths (falling back to the literal path) and compare.
fn same_file_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

/// File modification time in whole seconds since the Unix epoch.
#[must_use]
pub fn file_modified_epoch_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs())
}

/// Decide whether a connected server is stale relative to the installed binary.
///
/// A server is stale when either its executable path differs from the installed
/// one (a new install landed at a different location), or the installed binary
/// was modified after the running process started (an in-place rebuild). Pure
/// and platform-neutral so the decision is unit-tested without a live socket.
#[must_use]
pub fn stale_server_reason(
    running: &ConnectedServerInfo,
    installed_server_exe: &Path,
    installed_modified_secs: Option<u64>,
) -> Option<String> {
    if let Some(exe_path) =
        running.exe_path.as_deref().filter(|path| !same_file_path(path, installed_server_exe))
    {
        return Some(format!(
            "running server path {} differs from installed {}",
            exe_path.display(),
            installed_server_exe.display()
        ));
    }

    match (running.start_time_secs, installed_modified_secs) {
        (Some(start_time), Some(modified)) if modified > start_time => Some(format!(
            "installed server binary modified at {modified} after running server started at {start_time}"
        )),
        _ => None,
    }
}

/// Connect to the local server socket, starting the server if it is not yet
/// running and waiting up to [`SERVER_STARTUP_TIMEOUT`] for it to accept.
///
/// # Errors
/// Returns an error when the server cannot be started or does not become
/// connectable before the timeout.
pub async fn connect_or_start_server(
    socket_path: &Path,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
        return Ok(stream);
    }

    tracing::info!("server not running, starting scribe-server");
    platform_start_server()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    wait_for_server_connection(socket_path, SERVER_STARTUP_TIMEOUT).await
}

/// Poll the socket until it accepts a connection or `timeout` elapses.
///
/// # Errors
/// Returns an error if the server does not accept within `timeout`.
pub async fn wait_for_server_connection(
    socket_path: &Path,
    timeout: Duration,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            tracing::info!("connected to scribe-server");
            return Ok(stream);
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "scribe-server did not become ready within {}s",
                timeout.as_secs()
            )
            .into());
        }
    }
}

/// Block (synchronously) until the server socket is connectable or `timeout`
/// elapses. Used after a cold restart, before the async connect flow resumes.
///
/// # Errors
/// Returns an error if the socket is not connectable within `timeout`.
pub fn wait_for_server_ready(timeout: Duration) -> Result<(), String> {
    let socket_path = server_socket_path();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "scribe-server did not become ready within {}s after restart",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(SERVER_RETRY_INTERVAL);
    }
}

/// Start the local server. Linux uses the systemd user service; other platforms
/// are unsupported here (macOS launchd is deferred).
///
/// # Errors
/// Returns an error when the service cannot be started.
pub fn platform_start_server() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux_start_server()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(String::from("server auto-start is only supported on Linux in the GPUI client"))
    }
}

/// Push the current GUI environment into the systemd user manager so a server
/// it starts inherits `DISPLAY`/`WAYLAND_DISPLAY`/etc., and clears any that are
/// absent so a stale value from a previous session is not reused.
#[cfg(target_os = "linux")]
pub fn sync_linux_service_environment() {
    const GUI_ENV_VARS: &[&str] = &[
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
    ];

    let present: Vec<&str> =
        GUI_ENV_VARS.iter().copied().filter(|name| std::env::var_os(name).is_some()).collect();
    if !present.is_empty() {
        match std::process::Command::new("systemctl")
            .arg("--user")
            .arg("import-environment")
            .args(&present)
            .status()
        {
            Ok(status) if status.success() => {
                tracing::debug!(vars = ?present, "refreshed user systemd GUI environment");
            }
            Ok(status) => {
                tracing::warn!(vars = ?present, %status, "systemctl import-environment failed");
            }
            Err(e) => {
                tracing::warn!(vars = ?present, "failed to run systemctl import-environment: {e}");
            }
        }
    }

    let missing: Vec<&str> =
        GUI_ENV_VARS.iter().copied().filter(|name| std::env::var_os(name).is_none()).collect();
    if !missing.is_empty() {
        match std::process::Command::new("systemctl")
            .arg("--user")
            .arg("unset-environment")
            .args(&missing)
            .status()
        {
            Ok(status) if status.success() => {
                tracing::debug!(vars = ?missing, "cleared absent GUI vars from user systemd env");
            }
            Ok(status) => {
                tracing::warn!(vars = ?missing, %status, "systemctl unset-environment failed");
            }
            Err(e) => {
                tracing::warn!(vars = ?missing, "failed to run systemctl unset-environment: {e}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_start_server() -> Result<(), String> {
    sync_linux_service_environment();
    let identity = current_identity();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", identity.systemd_service_name()])
        .status()
        .map_err(|e| format!("failed to run systemctl: {e}"))?;
    if status.success() {
        tracing::info!(service = identity.systemd_service_name(), "server service started");
        Ok(())
    } else {
        Err(format!("systemctl start exited with {status}"))
    }
}

/// Whether any server process owned by `uid` matching `server_exe` is alive.
///
/// # Errors
/// Returns an error if `pgrep` cannot be run.
#[cfg(target_os = "linux")]
pub fn linux_server_processes_running(uid: &str, server_exe: &str) -> Result<bool, String> {
    let status = std::process::Command::new("pgrep")
        .args(["-u", uid, "-f", server_exe])
        .status()
        .map_err(|e| format!("failed to run pgrep for server processes: {e}"))?;
    Ok(status.success())
}

/// Force a cold restart of the local server.
///
/// Reloads and stops the systemd unit, kills any surviving server process
/// (escalating to SIGKILL), removes the stale IPC and handoff sockets, resets
/// the failed unit, then starts a fresh server and waits for it to accept. This
/// is the recovery path for a wedged server that still holds the lock but whose
/// accept loop has died. `client_exe` locates the sibling server binary.
///
/// # Errors
/// Returns an error if the fresh server cannot be started or does not become
/// ready within [`SERVER_STARTUP_TIMEOUT`].
#[cfg(target_os = "linux")]
pub fn perform_linux_cold_restart(client_exe: &Path) -> Result<(), String> {
    let identity = current_identity();
    let service_name = identity.systemd_service_name();
    let server_exe = client_exe.with_file_name(identity.server_binary_name());
    let server_exe_str = server_exe.to_string_lossy().into_owned();
    let uid = scribe_common::socket::current_uid().to_string();

    sync_linux_service_environment();

    drop(std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status());
    drop(std::process::Command::new("systemctl").args(["--user", "stop", service_name]).status());

    drop(
        std::process::Command::new("pkill")
            .args(["-u", uid.as_str(), "-f", server_exe_str.as_str()])
            .status(),
    );

    for _ in 0..10 {
        if !linux_server_processes_running(uid.as_str(), server_exe_str.as_str())? {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if linux_server_processes_running(uid.as_str(), server_exe_str.as_str())? {
        drop(
            std::process::Command::new("pkill")
                .args(["-9", "-u", uid.as_str(), "-f", server_exe_str.as_str()])
                .status(),
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    drop(std::fs::remove_file(server_socket_path()));
    drop(std::fs::remove_file(handoff_socket_path()));

    drop(
        std::process::Command::new("systemctl")
            .args(["--user", "reset-failed", service_name])
            .status(),
    );

    platform_start_server()?;
    wait_for_server_ready(SERVER_STARTUP_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(exe: &str, start: Option<u64>) -> ConnectedServerInfo {
        ConnectedServerInfo { pid: 42, exe_path: Some(PathBuf::from(exe)), start_time_secs: start }
    }

    // @lat: [[test#Server lifecycle#Path drift marks server stale]]
    #[test]
    fn stale_reason_detects_path_drift() {
        let info = running("/opt/old/scribe-server", Some(100));
        let reason = stale_server_reason(&info, Path::new("/usr/bin/scribe-server"), Some(100));
        assert!(reason.is_some());
    }

    // @lat: [[test#Server lifecycle#Newer installed binary marks server stale]]
    #[test]
    fn stale_reason_detects_newer_installed_binary() {
        let info = running("/usr/bin/scribe-server", Some(100));
        let reason = stale_server_reason(&info, Path::new("/usr/bin/scribe-server"), Some(101));
        assert!(reason.is_some());
    }

    // @lat: [[test#Server lifecycle#Matching fresh server is not stale]]
    #[test]
    fn stale_reason_ignores_matching_fresh_server() {
        let info = running("/usr/bin/scribe-server", Some(101));
        let reason = stale_server_reason(&info, Path::new("/usr/bin/scribe-server"), Some(100));
        assert!(reason.is_none());
    }

    // @lat: [[test#Server lifecycle#Unknown timestamps are not stale]]
    #[test]
    fn stale_reason_without_timestamps_is_fresh() {
        let info = running("/usr/bin/scribe-server", None);
        let reason = stale_server_reason(&info, Path::new("/usr/bin/scribe-server"), None);
        assert!(reason.is_none());
    }
}
