//! Local `scribe-server` lifecycle management for the GPUI client.
//!
//! The client owns starting, refreshing, and cold-restarting the per-user
//! server it talks to over the local IPC socket. Ported from the legacy
//! client's `ipc_client.rs` server-lifecycle helpers:
//!
//! - **Auto-start:** [`connect_or_start_server`] connects to the socket, or
//!   starts the platform service manager and waits for the socket to appear.
//! - **Stale-server refresh:** [`stale_server_reason`] is the pure decision
//!   that flags a connected server whose on-disk binary drifted (different
//!   path, or rebuilt after the running process started) so the caller can
//!   request a refresh.
//! - **Cold-restart recovery:** platform-specific fallbacks force-stop surviving
//!   processes, clear stale socket files, and start a fresh server when a wedged
//!   server holds the lock but its accept loop is dead.
//!
//! The IPC protocol and server binary are frozen; this module only spawns and
//! signals the existing server.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use scribe_common::app::current_identity;
#[cfg(any(target_os = "macos", test))]
use scribe_common::macos_launchd;
use scribe_common::macos_launchd::LaunchdSlot;
use scribe_common::socket::{handoff_socket_path, server_socket_path};

/// Maximum time to wait for a freshly started server to accept connections.
pub const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Detached client mode that completes an approved destructive update fallback.
pub const FINISH_UPDATE_RESTART_ARG: &str = "--finish-update-restart";
/// One-shot relay mode used by the macOS updater after swapping the app bundle.
pub const RELAUNCH_CLIENTS_ARG_PREFIX: &str =
    scribe_common::macos_launchd::RELAUNCH_CLIENTS_ARG_PREFIX;
use scribe_common::macos_launchd::TrackedClient;
/// Maximum time to wait for a hot-reloaded macOS server to take over.
#[cfg(target_os = "macos")]
const SERVER_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// launchd slot named by the process command line, when present.
    pub launchd_slot: Option<LaunchdSlot>,
    /// Whether the process command line was available for slot inspection.
    pub command_line_observed: bool,
}

/// A live local connection plus any deferred destructive action it requires.
pub struct ServerConnection {
    pub stream: tokio::net::UnixStream,
    pub cold_restart_required: bool,
}

/// Failure to establish a local server connection.
#[derive(Debug)]
pub struct ServerConnectError {
    pub reason: String,
    pub cold_restart_required: bool,
}

impl std::fmt::Display for ServerConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for ServerConnectError {}

/// Canonicalize both paths (falling back to the literal path) and compare.
fn same_file_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

/// Installed-file freshness time in whole seconds since the Unix epoch.
///
/// macOS installer copies can preserve a release artifact's modification time.
/// Its inode change time still advances on replacement, so macOS takes the
/// later value; other platforms retain the existing modification-time rule.
#[must_use]
pub fn file_change_epoch_secs(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(target_os = "macos")]
    let changed = {
        use std::os::unix::fs::MetadataExt as _;

        u64::try_from(metadata.ctime()).ok().map(|seconds| {
            let nanoseconds = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
            UNIX_EPOCH + Duration::new(seconds, nanoseconds)
        })
    };
    #[cfg(not(target_os = "macos"))]
    let changed = None;
    latest_file_change_epoch_secs(metadata.modified().ok(), changed)
}

fn latest_file_change_epoch_secs(
    modified: Option<std::time::SystemTime>,
    changed: Option<std::time::SystemTime>,
) -> Option<u64> {
    [modified, changed]
        .into_iter()
        .flatten()
        .max()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
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

/// Explain why connecting to `socket_path` failed, in the terms the user has to
/// act on.
///
/// The distinction that matters is *missing* versus *stale*: no socket file at
/// all means no server has ever run for this user, while a socket file that
/// refuses connections is the leftover of a server that died without unlinking
/// it — the case that used to present as an unexplained "connection refused".
/// Pure so both branches are unit-tested without a live socket.
#[must_use]
pub fn socket_failure_reason(socket_path: &Path, error: &std::io::Error) -> String {
    let path = socket_path.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            format!("no server socket at {path}; no scribe-server is running for this user")
        }
        std::io::ErrorKind::ConnectionRefused => format!(
            "stale server socket at {path}: the file exists but nothing is listening on it \
             (a previous scribe-server exited without removing it)"
        ),
        std::io::ErrorKind::PermissionDenied => {
            format!("no permission to connect to the server socket at {path}")
        }
        _ => format!("cannot connect to the server socket at {path}: {error}"),
    }
}

/// Connect to the local server socket, starting the server if it is not yet
/// running and waiting up to [`SERVER_STARTUP_TIMEOUT`] for it to accept.
///
/// A failed first connect is diagnosed through [`socket_failure_reason`] before
/// the autostart is attempted, and that diagnosis is carried into the returned
/// error when the autostart itself fails — so a wedged socket is named on the
/// status bar rather than surfacing as a bare `systemctl` exit code.
///
/// # Errors
/// Returns an error when the server cannot be started or does not become
/// connectable before the timeout.
pub async fn connect_or_start_server(
    socket_path: &Path,
) -> Result<ServerConnection, ServerConnectError> {
    let refusal = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(stream) => {
            #[cfg(target_os = "macos")]
            match refresh_stale_connected_server(&stream) {
                Ok(Some(refresh)) => {
                    if !refresh.started {
                        tracing::warn!(reason = %refresh.reason, "warm refresh could not start");
                        return Ok(ServerConnection { stream, cold_restart_required: false });
                    }
                    drop(stream);
                    tracing::info!(
                        old_pid = refresh.old_pid,
                        reason = %refresh.reason,
                        "waiting for refreshed scribe-server"
                    );
                    return wait_for_refreshed_server(
                        socket_path,
                        refresh.old_pid,
                        SERVER_REFRESH_TIMEOUT,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "failed to verify running server freshness; using existing connection"
                    );
                }
            }
            return Ok(ServerConnection { stream, cold_restart_required: false });
        }
        Err(error) => socket_failure_reason(socket_path, &error),
    };

    tracing::info!(%refusal, "server not running, starting scribe-server");
    platform_start_server().map_err(|error| ServerConnectError {
        cold_restart_required: error.cold_restart_required,
        reason: format!("{refusal}; autostart failed: {}", error.reason),
    })?;

    wait_for_server_connection(socket_path, SERVER_STARTUP_TIMEOUT).await.map_err(|error| {
        ServerConnectError {
            cold_restart_required: error.cold_restart_required,
            reason: format!("{refusal}; after autostart: {}", error.reason),
        }
    })
}

/// Snapshot the server process on the far end of a connected socket.
///
/// The peer PID comes from the kernel (`SO_PEERCRED` on Linux, `getpeereid`'s
/// PID equivalent elsewhere via `sysinfo`), so it names the process actually
/// serving this connection rather than whatever the pid file claims.
///
/// # Errors
/// Returns an error when the peer credentials cannot be read.
pub fn connected_server_info(
    stream: &tokio::net::UnixStream,
) -> Result<ConnectedServerInfo, String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    #[cfg(target_os = "macos")]
    let pid = peer_pid_of(stream)?;
    #[cfg(not(target_os = "macos"))]
    let pid = {
        let cred =
            stream.peer_cred().map_err(|e| format!("failed to read server peer cred: {e}"))?;
        cred.pid().ok_or_else(|| String::from("server peer cred carried no pid"))?
    };

    let sys_pid = Pid::from(usize::try_from(pid).unwrap_or(0));
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_cmd(UpdateKind::Always),
    );
    let process = system.process(sys_pid);
    let command_line_observed = process.is_some_and(|process| !process.cmd().is_empty());
    Ok(ConnectedServerInfo {
        pid,
        exe_path: process.and_then(|proc| proc.exe()).map(Path::to_path_buf),
        start_time_secs: process.map(sysinfo::Process::start_time),
        launchd_slot: process.and_then(|process| {
            LaunchdSlot::from_args(process.cmd().iter().map(|arg| arg.to_string_lossy()))
        }),
        command_line_observed,
    })
}

/// The `scribe-server` binary installed next to this client executable.
///
/// The client and the server ship in the same package, so the sibling path is
/// the installed server by construction — that is what a running server is held
/// up against by [`stale_server_reason`].
///
/// # Errors
/// Returns an error when this process's own executable path is unknown.
pub fn installed_server_exe() -> Result<PathBuf, String> {
    let client_exe =
        std::env::current_exe().map_err(|e| format!("cannot resolve client exe: {e}"))?;
    Ok(installed_bundle_executable(&client_exe, current_identity().server_binary_name()))
}

/// Resolve a shipped executable from the installed app rather than a renamed
/// `.app.prev` bundle that may still own the running process image.
fn installed_bundle_executable(current_exe: &Path, binary_name: &str) -> PathBuf {
    if let Some(previous_bundle) = current_exe.ancestors().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".app.prev"))
    }) {
        return previous_bundle.with_extension("").join("Contents/MacOS").join(binary_name);
    }

    current_exe.with_file_name(binary_name)
}

fn installed_client_exe() -> Result<PathBuf, String> {
    let current_exe = std::env::current_exe()
        .map_err(|error| format!("cannot resolve client executable: {error}"))?;
    Ok(installed_bundle_executable(&current_exe, current_identity().client_binary_name()))
}

/// Diagnose the server on the far end of a live connection.
///
/// Returns the human-readable staleness reason when the connected server no
/// longer matches the installed binary, and `None` when it is current or the
/// comparison cannot be made. Never fails the caller: a client that cannot read
/// `/proc` still works, it just cannot warn.
#[must_use]
pub fn connected_server_staleness(stream: &tokio::net::UnixStream) -> Option<String> {
    let installed = installed_server_exe()
        .inspect_err(|error| tracing::debug!(%error, "cannot locate the installed server binary"))
        .ok()?;
    let running = connected_server_info(stream)
        .inspect_err(|error| tracing::debug!(%error, "cannot inspect the connected server"))
        .ok()?;
    let installed_modified = file_change_epoch_secs(&installed);
    let reason = stale_server_reason(&running, &installed, installed_modified)?;
    tracing::warn!(pid = running.pid, %reason, "connected scribe-server is stale");
    Some(reason)
}

/// Poll the socket until it accepts a connection or `timeout` elapses.
///
/// # Errors
/// Returns an error if the server does not accept within `timeout`.
pub async fn wait_for_server_connection(
    socket_path: &Path,
    timeout: Duration,
) -> Result<ServerConnection, ServerConnectError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            #[cfg(target_os = "macos")]
            match refresh_stale_connected_server(&stream) {
                Ok(Some(refresh)) => {
                    if !refresh.started {
                        tracing::warn!(reason = %refresh.reason, "warm refresh could not start");
                        return Ok(ServerConnection { stream, cold_restart_required: false });
                    }
                    tracing::info!(
                        old_pid = refresh.old_pid,
                        reason = %refresh.reason,
                        "stale server reconnected during wait; switching to refresh-wait"
                    );
                    drop(stream);
                    return Box::pin(wait_for_refreshed_server(
                        socket_path,
                        refresh.old_pid,
                        SERVER_REFRESH_TIMEOUT,
                    ))
                    .await;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "failed to verify running server freshness while waiting; using existing connection"
                    );
                }
            }
            tracing::info!("connected to scribe-server");
            return Ok(ServerConnection { stream, cold_restart_required: false });
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(ServerConnectError {
                reason: format!("scribe-server did not become ready within {}s", timeout.as_secs()),
                cold_restart_required: false,
            });
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

/// Start the local server through systemd on Linux or launchd on macOS.
///
/// # Errors
/// Returns an error when the service cannot be started.
pub fn platform_start_server() -> Result<(), ServerConnectError> {
    #[cfg(target_os = "linux")]
    {
        linux_start_server()
    }
    #[cfg(target_os = "macos")]
    {
        macos_start_server()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ServerConnectError {
            reason: String::from("server auto-start is not supported on this platform"),
            cold_restart_required: false,
        })
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
fn linux_start_server() -> Result<(), ServerConnectError> {
    sync_linux_service_environment();
    let identity = current_identity();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", identity.systemd_service_name()])
        .status()
        .map_err(|error| ServerConnectError {
            reason: format!("failed to run systemctl: {error}"),
            cold_restart_required: false,
        })?;
    if status.success() {
        tracing::info!(service = identity.systemd_service_name(), "server service started");
        Ok(())
    } else {
        Err(ServerConnectError {
            reason: format!("systemctl start exited with {status}"),
            cold_restart_required: false,
        })
    }
}

#[cfg(target_os = "macos")]
fn macos_start_server() -> Result<(), ServerConnectError> {
    let identity = current_identity();
    let stale = listed_process_pids(identity.server_binary_name())
        .map_err(|reason| ServerConnectError { reason, cold_restart_required: false })?;
    if !stale.is_empty() {
        return Err(ServerConnectError {
            reason: format!(
                "scribe-server processes {stale:?} are alive without a reachable socket; cold restart required"
            ),
            cold_restart_required: true,
        });
    }
    let server_exe = installed_server_exe()
        .map_err(|reason| ServerConnectError { reason, cold_restart_required: false })?;
    if !server_exe.is_file() {
        return Err(ServerConnectError {
            reason: format!("server binary not found at {}", server_exe.display()),
            cold_restart_required: false,
        });
    }
    macos_launchd::activate_initial_slot(identity, LaunchdSlot::Primary)
        .map_err(|reason| ServerConnectError { reason, cold_restart_required: false })
}

#[cfg(target_os = "macos")]
fn restart_server(active_slot: LaunchdSlot) -> Result<(), String> {
    let identity = current_identity();
    let server_exe = installed_server_exe()?;
    if !server_exe.is_file() {
        return Err(format!("server binary not found at {}", server_exe.display()));
    }
    macos_launchd::activate_replacement(identity, active_slot)
}

/// Resolve the active slot without treating unreadable process metadata as a
/// legacy primary server.
#[cfg(any(target_os = "macos", test))]
fn refresh_active_slot(
    command_line_slot: Option<LaunchdSlot>,
    marker_slot: Option<LaunchdSlot>,
    command_line_observed: bool,
) -> Option<LaunchdSlot> {
    command_line_slot
        .or(marker_slot)
        .or_else(|| command_line_observed.then_some(LaunchdSlot::Primary))
}

#[cfg(any(target_os = "macos", test))]
fn marker_binary_drift(
    marker_binary: Option<macos_launchd::BinaryIdentity>,
    installed_binary: Option<macos_launchd::BinaryIdentity>,
) -> bool {
    matches!((marker_binary, installed_binary), (Some(running), Some(installed)) if running != installed)
}

#[cfg(target_os = "macos")]
fn peer_pid_of(stream: &tokio::net::UnixStream) -> Result<i32, String> {
    use nix::sys::socket::{getsockopt, sockopt};

    getsockopt(stream, sockopt::LocalPeerPid)
        .map_err(|error| format!("failed to query server peer pid: {error}"))
}

/// Parse the updater relay's old-server and exact client PID payload.
#[must_use]
pub fn client_relaunch_request<I, S>(args: I) -> Option<(Option<u32>, Vec<TrackedClient>)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let payload = args.into_iter().find_map(|argument| {
        argument.as_ref().strip_prefix(RELAUNCH_CLIENTS_ARG_PREFIX).map(str::to_owned)
    })?;
    let (server, clients) = payload.split_once(':')?;
    let old_server = if server == "unchanged" { None } else { Some(server.parse().ok()?) };
    let clients = if clients.is_empty() {
        Vec::new()
    } else {
        clients
            .split(',')
            .map(|client| {
                let (pid, start_time) = client.split_once('@')?;
                Some(TrackedClient {
                    pid: pid.parse().ok()?,
                    start_time_secs: start_time.parse().ok()?,
                })
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some((old_server, clients))
}

#[cfg(target_os = "macos")]
fn socket_peer_pid() -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt};

    let stream = std::os::unix::net::UnixStream::connect(server_socket_path()).ok()?;
    u32::try_from(getsockopt(&stream, sockopt::LocalPeerPid).ok()?).ok()
}

#[cfg(target_os = "macos")]
fn wait_for_replacement_server(
    old_server_pid: Option<u32>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let ready = socket_peer_pid().is_some_and(|pid| {
            old_server_pid != Some(pid)
                && (old_server_pid.is_none()
                    || macos_launchd::active_slot_for_pid(current_identity(), pid).is_some())
        });
        if ready {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(String::from(
                "replacement server did not become ready for client relaunch",
            ));
        }
        std::thread::sleep(SERVER_RETRY_INTERVAL);
    }
}

#[cfg(target_os = "macos")]
fn request_graceful_client_shutdown() -> Result<(), String> {
    use std::io::Write as _;

    let mut stream = std::os::unix::net::UnixStream::connect(server_socket_path())
        .map_err(|error| format!("cannot connect for graceful client shutdown: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("cannot bound graceful client shutdown write: {error}"))?;
    let payload = rmp_serde::to_vec_named(&scribe_common::protocol::ClientMessage::QuitAll)
        .map_err(|error| format!("cannot encode graceful client shutdown: {error}"))?;
    let length = u32::try_from(payload.len())
        .map_err(|_| String::from("graceful client shutdown frame is too large"))?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&payload))
        .map_err(|error| format!("cannot send graceful client shutdown: {error}"))
}

#[cfg(target_os = "macos")]
fn request_settings_shutdown() -> Result<(), String> {
    use std::io::Write as _;

    let path = scribe_common::socket::settings_socket_path();
    let mut stream = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("cannot connect to settings shutdown socket: {error}")),
    };
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("cannot bound settings shutdown write: {error}"))?;
    stream
        .write_all(b"{\"cmd\":\"quit\"}\n")
        .map_err(|error| format!("cannot request settings shutdown: {error}"))
}

/// Relaunch changed clients only after the replacement server is accepting.
#[cfg(target_os = "macos")]
pub fn finish_client_relaunch(
    old_server_pid: Option<u32>,
    tracked_clients: &[TrackedClient],
) -> Result<(), String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    if tracked_clients.is_empty() {
        return Ok(());
    }
    wait_for_replacement_server(old_server_pid, SERVER_REFRESH_TIMEOUT)?;

    let name = current_identity().client_binary_name();
    let currently_named = listed_process_pids(name)?;
    let sys_pids =
        tracked_clients.iter().map(|client| Pid::from_u32(client.pid)).collect::<Vec<_>>();
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&sys_pids),
        true,
        ProcessRefreshKind::nothing(),
    );
    let targets = tracked_clients
        .iter()
        .copied()
        .filter(|client| {
            currently_named.contains(&client.pid)
                && system
                    .process(Pid::from_u32(client.pid))
                    .is_some_and(|process| process.start_time() == client.start_time_secs)
        })
        .collect::<Vec<_>>();
    request_settings_shutdown()?;
    request_graceful_client_shutdown()?;
    let graceful_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut survivors = targets;
    while !survivors.is_empty() && std::time::Instant::now() < graceful_deadline {
        std::thread::sleep(SERVER_RETRY_INTERVAL);
        survivors.retain(|client| process_is_alive(client.pid));
    }

    if !survivors.is_empty() {
        let pids = survivors.iter().map(|client| client.pid).collect::<Vec<_>>();
        return Err(format!(
            "pre-update clients {pids:?} did not exit after graceful shutdown; refusing destructive termination and duplicate relaunch"
        ));
    }

    spawn_replacement_client(&installed_client_exe()?)
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct StaleRefresh {
    old_pid: i32,
    reason: String,
    started: bool,
}

#[cfg(target_os = "macos")]
fn refresh_stale_connected_server(
    stream: &tokio::net::UnixStream,
) -> Result<Option<StaleRefresh>, String> {
    let installed = installed_server_exe()?;
    let running = connected_server_info(stream)?;
    let installed_modified = file_change_epoch_secs(&installed);
    let marker_record = u32::try_from(running.pid)
        .ok()
        .and_then(|pid| macos_launchd::active_slot_record_for_pid(current_identity(), pid));
    let reason = stale_server_reason(&running, &installed, installed_modified).or_else(|| {
        marker_binary_drift(
            marker_record.and_then(|(_, binary)| binary),
            macos_launchd::binary_identity(&installed),
        )
        .then(|| String::from("running server executable identity differs from installed bundle"))
    });
    let Some(reason) = reason else {
        return Ok(None);
    };

    tracing::info!(
        pid = running.pid,
        %reason,
        "connected scribe-server is stale; requesting refresh"
    );
    let marker_slot = marker_record.map(|(slot, _)| slot);
    let Some(active_slot) =
        refresh_active_slot(running.launchd_slot, marker_slot, running.command_line_observed)
    else {
        return Ok(Some(StaleRefresh {
            old_pid: running.pid,
            reason: format!(
                "{reason}; warm refresh refused because the active launchd slot could not be proven"
            ),
            started: false,
        }));
    };
    match restart_server(active_slot) {
        Ok(()) => Ok(Some(StaleRefresh { old_pid: running.pid, reason, started: true })),
        Err(error) => Ok(Some(StaleRefresh {
            old_pid: running.pid,
            reason: format!("{reason}; warm launchd activation failed: {error}"),
            started: false,
        })),
    }
}

/// Wait until a hot-upgrade replaces `old_pid`, then return its connection.
#[cfg(target_os = "macos")]
async fn wait_for_refreshed_server(
    socket_path: &Path,
    old_pid: i32,
    timeout: Duration,
) -> Result<ServerConnection, ServerConnectError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            match peer_pid_of(&stream) {
                Ok(pid) if pid != old_pid => {
                    tracing::info!(new_pid = pid, "connected to refreshed scribe-server");
                    return Ok(ServerConnection { stream, cold_restart_required: false });
                }
                Ok(_) => drop(stream),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "could not query peer pid after refresh; accepting connection"
                    );
                    return Ok(ServerConnection { stream, cold_restart_required: false });
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                old_pid,
                "handoff did not take over; user-approved cold restart required"
            );
            let stream = tokio::net::UnixStream::connect(socket_path).await.map_err(|error| {
                ServerConnectError {
                    reason: format!("handoff failed and the old server is unreachable: {error}"),
                    cold_restart_required: true,
                }
            })?;
            return Ok(ServerConnection { stream, cold_restart_required: true });
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn listed_process_pids(process_name: &str) -> Result<Vec<u32>, String> {
    let mut command = std::process::Command::new("pgrep");
    #[cfg(target_os = "linux")]
    command.args(["-x", process_name]);
    #[cfg(target_os = "macos")]
    {
        let uid = scribe_common::socket::current_uid().to_string();
        command.args(["-U", uid.as_str(), "-x", process_name]);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to run pgrep for {process_name}: {error}"))?;

    if !output.status.success() {
        return if output.status.code() == Some(1) {
            Ok(Vec::new())
        } else {
            Err(format!("pgrep for {process_name} exited with {}", output.status))
        };
    }

    Ok(output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok()?.trim().parse::<u32>().ok())
        .collect())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_is_alive(pid: u32) -> bool {
    let signalable = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success());
    signalable && !process_is_zombie(pid)
}

fn process_state_is_zombie(state: &str) -> bool {
    state.trim_start().starts_with('Z')
}

#[cfg(target_os = "linux")]
fn process_is_zombie(pid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status.lines().find_map(|line| line.strip_prefix("State:")).is_some_and(process_state_is_zombie)
}

#[cfg(target_os = "macos")]
fn process_is_zombie(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|state| process_state_is_zombie(&state))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(SERVER_RETRY_INTERVAL);
    }
    !process_is_alive(pid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn surviving_process_pids(pids: &[u32], mut is_alive: impl FnMut(u32) -> bool) -> Vec<u32> {
    pids.iter().copied().filter(|pid| is_alive(*pid)).collect()
}

/// Return whether this invocation is the detached deferred-restart helper.
#[must_use]
pub fn is_finish_update_restart<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|arg| arg.as_ref() == FINISH_UPDATE_RESTART_ARG)
}

/// Spawn the installed client in detached deferred-restart mode.
///
/// # Errors
/// Returns an error when this executable cannot be resolved or the helper
/// process cannot be started.
pub fn spawn_update_restart_helper() -> Result<(), String> {
    use std::process::Stdio;

    let client_exe = installed_client_exe()?;
    let child = std::process::Command::new(&client_exe)
        .arg(FINISH_UPDATE_RESTART_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("failed to spawn deferred update helper {}: {error}", client_exe.display())
        })?;

    tracing::info!(pid = child.id(), exe = %client_exe.display(), "spawned deferred update helper");
    Ok(())
}

fn other_process_pids(current_pid: u32, pids: impl IntoIterator<Item = u32>) -> Vec<u32> {
    pids.into_iter().filter(|pid| *pid != current_pid).collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn acquire_restart_helper_guard(
    identity: scribe_common::app::AppIdentity,
) -> Result<Option<nix::fcntl::Flock<std::fs::File>>, String> {
    let directory =
        identity.state_dir().ok_or_else(|| String::from("state directory unavailable"))?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create restart-helper lock directory: {error}"))?;
    let path = directory.join("update-restart-helper.lock");
    let file =
        std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path).map_err(
            |error| format!("failed to open restart-helper lock {}: {error}", path.display()),
        )?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(guard) => Ok(Some(guard)),
        Err((_, error)) if error == nix::errno::Errno::EWOULDBLOCK => Ok(None),
        Err((_, error)) => {
            Err(format!("failed to acquire restart-helper lock {}: {error}", path.display()))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_tracked_clients_to_exit(
    process_name: &str,
    client_pids: &[u32],
    cooperative_timeout: Duration,
) -> Result<(), String> {
    let cooperative_deadline = std::time::Instant::now() + cooperative_timeout;
    let mut survivors = surviving_process_pids(client_pids, process_is_alive);
    while !survivors.is_empty() && std::time::Instant::now() < cooperative_deadline {
        std::thread::sleep(SERVER_RETRY_INTERVAL);
        survivors = surviving_process_pids(&survivors, process_is_alive);
    }

    let still_named = listed_process_pids(process_name)?;
    survivors.retain(|pid| still_named.contains(pid));

    // The ordinary path exits through the server's QuitRequested broadcast.
    // If the server is unreachable, that broadcast cannot arrive; SIGTERM is
    // the client's explicit graceful-shutdown signal and flushes every hosted
    // window before exiting. Settings-only processes retain SIGTERM's default
    // action because they have no terminal restore state to flush.
    for pid in &survivors {
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .map_err(|error| format!("failed to signal client pid {pid}: {error}"))?;
        if !status.success() && process_is_alive(*pid) {
            return Err(format!("kill -TERM {pid} exited with {status}"));
        }
    }

    for pid in survivors {
        if !wait_for_process_exit(pid, Duration::from_secs(10)) {
            return Err(format!(
                "client pid {pid} did not exit after the deferred restart was approved"
            ));
        }
    }

    // Let the old server consume every connection EOF before it is stopped.
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn spawn_replacement_client(client_exe: &Path) -> Result<(), String> {
    use std::process::Stdio;

    let child = std::process::Command::new(client_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to relaunch client {}: {error}", client_exe.display()))?;
    tracing::info!(pid = child.id(), exe = %client_exe.display(), "relaunched client after update restart");
    Ok(())
}

fn restart_then_relaunch(
    restart: impl FnOnce() -> Result<(), String>,
    relaunch: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let restart_result = restart();
    let relaunch_result = relaunch();
    match (restart_result, relaunch_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(restart_error), Ok(())) => Err(format!(
            "server restart failed, but the client was relaunched for recovery: {restart_error}"
        )),
        (Ok(()), Err(relaunch_error)) => Err(relaunch_error),
        (Err(restart_error), Err(relaunch_error)) => Err(format!(
            "server restart failed: {restart_error}; client relaunch also failed: {relaunch_error}"
        )),
    }
}

/// Complete an update whose warm server handoff failed.
///
/// The UI starts this helper before asking every client window to save and
/// exit. The helper waits for those clients, cold-restarts the server, then
/// launches one fresh client so the normal restore fan-out recreates the rest.
///
/// # Errors
/// Returns an error if clients do not exit, the server restart fails, or the
/// replacement client cannot be launched.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn finish_update_restart() -> Result<(), String> {
    let identity = current_identity();
    let Some(_restart_guard) = acquire_restart_helper_guard(identity)? else {
        tracing::info!("another deferred update helper already owns the restart");
        return Ok(());
    };
    let client_exe = installed_client_exe()?;
    let clients = listed_process_pids(identity.client_binary_name())?;
    let client_pids = other_process_pids(std::process::id(), clients);
    let cooperative_timeout =
        if std::os::unix::net::UnixStream::connect(server_socket_path()).is_ok() {
            Duration::from_secs(10)
        } else {
            Duration::ZERO
        };

    wait_for_tracked_clients_to_exit(
        identity.client_binary_name(),
        &client_pids,
        cooperative_timeout,
    )?;

    #[cfg(target_os = "linux")]
    let restart = || perform_linux_cold_restart(&client_exe);

    #[cfg(target_os = "macos")]
    let restart = perform_macos_cold_restart;

    restart_then_relaunch(restart, || spawn_replacement_client(&client_exe))
}

/// Reject deferred update mode on unsupported platforms.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn finish_update_restart() -> Result<(), String> {
    Err(String::from("deferred update restart is only supported on macOS and Linux"))
}

#[cfg(target_os = "macos")]
fn terminate_pid(pid: u32, label: &str) -> Result<(), String> {
    let pid_text = pid.to_string();
    let status = std::process::Command::new("kill")
        .arg(&pid_text)
        .status()
        .map_err(|error| format!("failed to signal {label} pid {pid}: {error}"))?;
    if !status.success() && !process_is_alive(pid) {
        return Ok(());
    }
    if !status.success() {
        return Err(format!("kill {pid} for {label} exited with {status}"));
    }
    if wait_for_process_exit(pid, Duration::from_secs(5)) {
        return Ok(());
    }

    let force_status = std::process::Command::new("kill")
        .args(["-9", &pid_text])
        .status()
        .map_err(|error| format!("failed to force-kill {label} pid {pid}: {error}"))?;
    if !force_status.success() {
        return Err(format!("kill -9 {pid} for {label} exited with {force_status}"));
    }
    if wait_for_process_exit(pid, Duration::from_secs(1)) {
        Ok(())
    } else {
        Err(format!("timed out waiting for {label} pid {pid} to exit"))
    }
}

#[cfg(target_os = "macos")]
fn remove_file_if_present(path: &Path, description: &str) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to remove {description} {}: {error}", path.display())),
    }
}

/// Last-resort macOS recovery after a handoff or accept-loop failure.
#[cfg(target_os = "macos")]
fn perform_macos_cold_restart() -> Result<(), String> {
    let identity = current_identity();
    macos_launchd::unregister_all_slots(identity)?;
    let current_pid = std::process::id();
    let server_pids = listed_process_pids(identity.server_binary_name())?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect::<Vec<_>>();

    for pid in server_pids {
        terminate_pid(pid, "scribe-server")?;
    }

    let survivors = listed_process_pids(identity.server_binary_name())?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect::<Vec<_>>();
    if !survivors.is_empty() {
        return Err(format!(
            "refusing to clear server sockets while scribe-server processes {survivors:?} remain alive"
        ));
    }

    remove_file_if_present(&server_socket_path(), "server socket")?;
    remove_file_if_present(&handoff_socket_path(), "handoff socket")?;
    if let Some(path) = macos_launchd::active_slot_path(identity) {
        remove_file_if_present(&path, "active-slot marker")?;
    }

    let server_exe = installed_server_exe()?;
    if !server_exe.is_file() {
        return Err(format!("server binary not found at {}", server_exe.display()));
    }
    macos_launchd::activate_initial_slot(identity, LaunchdSlot::Primary)?;
    wait_for_server_ready(SERVER_STARTUP_TIMEOUT)
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

    platform_start_server().map_err(|error| error.reason)?;
    wait_for_server_ready(SERVER_STARTUP_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(exe: &str, start: Option<u64>) -> ConnectedServerInfo {
        ConnectedServerInfo {
            pid: 42,
            exe_path: Some(PathBuf::from(exe)),
            start_time_secs: start,
            launchd_slot: None,
            command_line_observed: true,
        }
    }

    // @lat: [[test#Test Harness#Server lifecycle#Unknown launchd slot fails closed]]
    #[test]
    fn unknown_launchd_slot_fails_closed() {
        assert_eq!(
            refresh_active_slot(Some(LaunchdSlot::Alternate), None, true),
            Some(LaunchdSlot::Alternate)
        );
        assert_eq!(
            refresh_active_slot(None, Some(LaunchdSlot::Alternate), false),
            Some(LaunchdSlot::Alternate)
        );
        assert_eq!(refresh_active_slot(None, None, true), Some(LaunchdSlot::Primary));
        assert_eq!(refresh_active_slot(None, None, false), None);
    }

    // @lat: [[test#Test Harness#Server lifecycle#Cold restart requirement is typed]]
    #[test]
    fn cold_restart_requirement_is_typed() {
        let error = ServerConnectError {
            reason: String::from("server is wedged"),
            cold_restart_required: true,
        };

        assert!(error.cold_restart_required);
        assert_eq!(error.to_string(), "server is wedged");
    }

    // @lat: [[test#Test Harness#Server lifecycle#Deferred restart uses the installed bundle]]
    #[test]
    fn deferred_restart_uses_the_installed_bundle() {
        let running = Path::new("/Applications/Scribe.app.prev/Contents/MacOS/scribe-client");
        let installed = installed_bundle_executable(running, "scribe-client");

        assert_eq!(
            installed,
            PathBuf::from("/Applications/Scribe.app/Contents/MacOS/scribe-client")
        );
    }

    // @lat: [[test#Server lifecycle#Launchd plist pins a baseline PATH]]
    #[test]
    fn launchd_plist_sets_baseline_path_environment() {
        let plist = macos_launchd::plist_contents(
            scribe_common::app::AppIdentity::stable(),
            LaunchdSlot::Primary,
        );
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains(
            "<key>PATH</key>\n\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>"
        ));
    }

    // @lat: [[test#Server lifecycle#Dist plist matches the generated plist]]
    #[test]
    fn dist_plist_matches_generated_launchd_plist() {
        let generated = macos_launchd::plist_contents(
            scribe_common::app::AppIdentity::stable(),
            LaunchdSlot::Primary,
        );
        assert_eq!(generated, include_str!("../../../dist/macos/com.scribe.server.plist"));

        let alternate = macos_launchd::plist_contents(
            scribe_common::app::AppIdentity::stable(),
            LaunchdSlot::Alternate,
        );
        assert_eq!(
            alternate,
            include_str!("../../../dist/macos/com.scribe.server.alternate.plist")
        );
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

    // @lat: [[test#Test Harness#Server lifecycle#Manual DMG inode change marks server stale]]
    #[test]
    fn later_inode_change_time_wins_over_preserved_mtime() {
        let modified = UNIX_EPOCH + Duration::from_secs(50);
        let changed = UNIX_EPOCH + Duration::from_mins(2);

        assert_eq!(latest_file_change_epoch_secs(Some(modified), Some(changed)), Some(120));
    }

    // @lat: [[test#Test Harness#Server lifecycle#Launchd marker detects bundle replacement]]
    #[test]
    fn launchd_marker_detects_bundle_replacement() {
        let running = macos_launchd::BinaryIdentity { device: 1, inode: 20 };
        let installed = macos_launchd::BinaryIdentity { device: 1, inode: 21 };

        assert!(marker_binary_drift(Some(running), Some(installed)));
        assert!(!marker_binary_drift(Some(running), Some(running)));
        assert!(!marker_binary_drift(None, Some(installed)));
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

    // @lat: [[test#Server lifecycle#macOS peer PID drives stale refresh]]
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_connected_server_info_reads_peer_pid() {
        let socket_path = std::env::temp_dir().join(format!(
            "scribe-peer-pid-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after the Unix epoch")
                .as_nanos()
        ));
        let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime should start");
        runtime.block_on(async {
            let listener =
                tokio::net::UnixListener::bind(&socket_path).expect("test socket should bind");
            let client = tokio::net::UnixStream::connect(&socket_path)
                .await
                .expect("test client should connect");
            let (_server, _) = listener.accept().await.expect("test server should accept");

            let info = connected_server_info(&client).expect("macOS should expose LOCAL_PEERPID");
            assert_eq!(info.pid, i32::try_from(std::process::id()).unwrap());
        });
        drop(std::fs::remove_file(socket_path));
    }

    // @lat: [[test#Server lifecycle#Missing socket is named as no server]]
    #[test]
    fn missing_socket_is_reported_as_no_server() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        let reason = socket_failure_reason(Path::new("/run/user/1000/scribe/server.sock"), &error);
        assert!(reason.contains("no server socket at /run/user/1000/scribe/server.sock"));
        assert!(!reason.contains("stale"));
    }

    // @lat: [[test#Server lifecycle#Refused socket is named as stale]]
    #[test]
    fn refused_socket_is_reported_as_stale() {
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let reason = socket_failure_reason(Path::new("/run/user/1000/scribe/server.sock"), &error);
        assert!(reason.contains("stale server socket"));
        assert!(reason.contains("nothing is listening"));
    }

    // @lat: [[test#Server lifecycle#Other connect failures keep the OS error]]
    #[test]
    fn other_connect_failures_keep_the_os_error() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let denied_reason = socket_failure_reason(Path::new("/tmp/s.sock"), &denied);
        assert!(denied_reason.contains("no permission"));

        let other = std::io::Error::other("boom");
        let other_reason = socket_failure_reason(Path::new("/tmp/s.sock"), &other);
        assert!(other_reason.contains("boom"));
    }

    // @lat: [[test#Test Harness#Server lifecycle#Deferred restart mode bypasses the UI]]
    #[test]
    fn deferred_restart_mode_bypasses_the_ui() {
        assert!(is_finish_update_restart(["scribe-client", FINISH_UPDATE_RESTART_ARG]));
        assert!(!is_finish_update_restart(["scribe-client", "--restore-child"]));
    }

    // @lat: [[test#Test Harness#Server lifecycle#Client relaunch relay payload is typed]]
    #[test]
    fn client_relaunch_relay_payload_is_typed() {
        assert_eq!(
            client_relaunch_request([
                "scribe-client",
                "--relaunch-clients-after-server=41:7@70,9@90",
            ]),
            Some((
                Some(41),
                vec![
                    TrackedClient { pid: 7, start_time_secs: 70 },
                    TrackedClient { pid: 9, start_time_secs: 90 },
                ],
            ))
        );
        assert_eq!(
            client_relaunch_request(["--relaunch-clients-after-server=unchanged:7@70"]),
            Some((None, vec![TrackedClient { pid: 7, start_time_secs: 70 }]))
        );
        assert_eq!(client_relaunch_request(["--relaunch-clients-after-server=bad:7@70"]), None);
    }

    // @lat: [[test#Test Harness#Server lifecycle#Deferred restart helper excludes itself]]
    #[test]
    fn deferred_restart_helper_excludes_itself() {
        assert_eq!(other_process_pids(42, [11, 42, 73]), vec![11, 73]);
    }

    // @lat: [[test#Test Harness#Server lifecycle#Deferred restart signals only surviving clients]]
    #[test]
    fn deferred_restart_signals_only_surviving_clients() {
        assert_eq!(surviving_process_pids(&[11, 42, 73], |pid| pid != 42), vec![11, 73]);
    }

    // @lat: [[test#Test Harness#Server lifecycle#Zombie clients do not block relaunch]]
    #[test]
    fn zombie_clients_do_not_block_relaunch() {
        assert!(process_state_is_zombie(" Z (zombie)"));
        assert!(process_state_is_zombie("Z+"));
        assert!(!process_state_is_zombie("S (sleeping)"));
    }

    // @lat: [[test#Test Harness#Server lifecycle#Restart failure still relaunches the client]]
    #[test]
    fn restart_failure_still_relaunches_the_client() {
        let relaunched = std::cell::Cell::new(false);
        let result = restart_then_relaunch(
            || Err(String::from("restart failed")),
            || {
                relaunched.set(true);
                Ok(())
            },
        );

        assert!(relaunched.get());
        assert!(result.is_err_and(|error| error.contains("relaunched for recovery")));
    }
}
