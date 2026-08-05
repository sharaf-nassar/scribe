use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use scribe_common::error::ScribeError;
use scribe_common::socket::{current_uid, server_socket_path};

/// Returns the path to the PID file used to track the running scribe-server
/// process. Stored in the user runtime directory alongside the server socket.
///
/// - Linux: `/run/user/{uid}/scribe/scribe-server.pid`
/// - macOS: `~/Library/Application Support/Scribe/run/scribe-server.pid`
fn pid_file_path() -> PathBuf {
    server_socket_path().parent().map_or_else(
        || PathBuf::from(format!("/run/user/{}/scribe/scribe-server.pid", current_uid())),
        |dir| dir.join("scribe-server.pid"),
    )
}

/// Maximum time to wait for the server socket to appear after spawning.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for the server socket.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum time to wait for the process to exit after SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Maximum time to wait for the old server to exit during a hot-reload upgrade.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var naming a file the spawned server's stdout/stderr is appended to.
///
/// Unset by default, so a normal run still discards server output. E2E scripts
/// that need to assert on server-side behaviour with no pixels and no client
/// state behind it — "the server received `TriggerUpdate` and started an
/// install" — point this at a file under `/output` and grep it, mirroring how
/// `docker/entrypoint-visual.sh` already persists the client's tracing output.
const SERVER_LOG_ENV: &str = "SCRIBE_TEST_SERVER_LOG";

/// Redirect target for the spawned server's stdout and stderr.
fn server_log_stdio() -> Result<Stdio, ScribeError> {
    let Some(path) = std::env::var_os(SERVER_LOG_ENV).filter(|value| !value.is_empty()) else {
        return Ok(Stdio::null());
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| ScribeError::Io { source: e })?;
    Ok(Stdio::from(file))
}

/// Start the scribe-server process in the background.
///
/// Spawns `scribe-server` as a detached child process, writes its PID to a
/// file, then polls until the server socket appears (or a timeout is reached).
pub async fn start() -> Result<(), ScribeError> {
    let child = std::process::Command::new("scribe-server")
        .stdout(server_log_stdio()?)
        .stderr(server_log_stdio()?)
        .spawn()
        .map_err(|e| ScribeError::IpcError {
            reason: format!("failed to spawn scribe-server: {e}"),
        })?;

    let pid = child.id();
    tokio::fs::write(pid_file_path(), pid.to_string())
        .await
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to write PID file: {e}") })?;

    wait_for_socket().await
}

/// Poll for the server socket to appear on disk.
async fn wait_for_socket() -> Result<(), ScribeError> {
    let socket_path = server_socket_path();
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;

    loop {
        if tokio::fs::try_exists(&socket_path).await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ScribeError::IpcError {
                reason: format!("timed out waiting for server socket at {}", socket_path.display()),
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Stop the scribe-server process.
///
/// Reads the PID from the PID file, sends `SIGTERM`, waits up to 3 seconds for
/// the process to exit, then sends `SIGKILL` if it is still running. The PID
/// file is removed before returning.
pub async fn stop() -> Result<(), ScribeError> {
    let pid_file = pid_file_path();
    let pid_str = tokio::fs::read_to_string(&pid_file)
        .await
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to read PID file: {e}") })?;

    let raw_pid: i32 = pid_str
        .trim()
        .parse()
        .map_err(|e| ScribeError::IpcError { reason: format!("invalid PID in file: {e}") })?;

    let pid = Pid::from_raw(raw_pid);

    send_signal_and_wait(pid).await?;

    tokio::fs::remove_file(&pid_file)
        .await
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to remove PID file: {e}") })?;

    Ok(())
}

/// Trigger a hot-reload upgrade.
///
/// Launches `scribe-server --upgrade` which connects to the running server's
/// handoff socket, receives session state + PTY fds, and takes over as the
/// new server. The old server exits after the handoff. The PID file is
/// updated to point to the new process.
pub async fn upgrade() -> Result<(), ScribeError> {
    let old_pid_str = tokio::fs::read_to_string(pid_file_path())
        .await
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to read PID file: {e}") })?;

    let old_pid: i32 = old_pid_str
        .trim()
        .parse()
        .map_err(|e| ScribeError::IpcError { reason: format!("invalid PID in file: {e}") })?;

    // Launch the new server with --upgrade. The successor inherits the same
    // log redirection as `start`, so a script asserting on server-side
    // behaviour does not go blind exactly at the hot-reload it is measuring.
    let child = std::process::Command::new("scribe-server")
        .arg("--upgrade")
        .stdout(server_log_stdio()?)
        .stderr(server_log_stdio()?)
        .spawn()
        .map_err(|e| ScribeError::IpcError {
            reason: format!("failed to spawn scribe-server --upgrade: {e}"),
        })?;

    let new_pid = child.id();

    // Wait for the old server to exit (it should exit after sending the handoff).
    let old_nix_pid = Pid::from_raw(old_pid);
    let deadline = tokio::time::Instant::now() + UPGRADE_TIMEOUT;

    loop {
        if kill(old_nix_pid, None).is_err() {
            break; // Old server exited.
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ScribeError::IpcError {
                reason: format!("old server (pid {old_pid}) did not exit within timeout"),
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Verify the new server's socket is available.
    wait_for_socket().await?;

    // Update PID file to point to the new process.
    tokio::fs::write(pid_file_path(), new_pid.to_string())
        .await
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to write PID file: {e}") })?;

    Ok(())
}

/// Send `SIGTERM` and wait for the process to exit, escalating to `SIGKILL`.
async fn send_signal_and_wait(pid: Pid) -> Result<(), ScribeError> {
    kill(pid, Signal::SIGTERM).map_err(|e| ScribeError::IpcError {
        reason: format!("failed to send SIGTERM to {pid}: {e}"),
    })?;

    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;

    loop {
        // Signal 0 checks whether the process still exists without sending a
        // real signal.  An error (typically ESRCH) means it has exited.
        if kill(pid, None).is_err() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Process is still alive after timeout — force kill.
    let _kill_result = kill(pid, Signal::SIGKILL);

    Ok(())
}
