//! Client-process singleton enforcement.
//!
//! Uses a Unix domain socket for singleton detection and a `flock` advisory
//! lock to prevent TOCTOU races during the bind-or-connect sequence.

use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use scribe_common::app::{AppIdentity, current_identity};
use scribe_common::ids::WindowId;
use scribe_common::settings_window::{SettingsWindowAnchor, SettingsWindowCommand};
use scribe_common::socket::{
    CLIENT_FOCUS_SOCKET_PREFIX, ClientFocusGeneration, client_focus_socket_path, client_lock_path,
    client_socket_path, current_uid, settings_lock_path, settings_socket_path,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const MAX_COMMAND_LINE_BYTES: usize = 4096;
const FOCUS_IO_TIMEOUT: Duration = Duration::from_millis(100);
const CLEANUP_LIMIT: usize = 64;
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);

/// Commands accepted by the terminal singleton listener.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum TerminalFocusCommand {
    Focus {
        #[serde(default)]
        anchor: Option<SettingsWindowAnchor>,
    },
    AnnounceActivation {
        generation: ClientFocusGeneration,
        endpoint: PathBuf,
        window_id: WindowId,
    },
}

/// Commands accepted by one restore-child focus endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum FocusEndpointRequest {
    Activate { generation: ClientFocusGeneration },
    Probe { socket_tag: String },
}

/// Typed response from one restore-child focus endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum FocusEndpointResult {
    Activated { generation: ClientFocusGeneration },
    Alive { generation: ClientFocusGeneration },
    Rejected { reason: FocusEndpointRejection },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusEndpointRejection {
    GenerationMismatch,
    UnavailableWindow,
    Unauthorized,
    Malformed,
}

#[derive(Debug, thiserror::Error)]
pub enum FocusTransportError {
    #[error("focus transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("focus transport timed out")]
    Timeout,
    #[error("focus transport frame was truncated")]
    Truncated,
    #[error("focus transport frame exceeded its size limit")]
    TooLarge,
    #[error("focus transport frame was malformed")]
    Malformed,
    #[error("focus transport received an unexpected command")]
    UnexpectedCommand,
    #[error("focus endpoint path is invalid")]
    InvalidEndpoint,
    #[error("focus endpoint generation does not match")]
    GenerationMismatch,
    #[error("focus peer was rejected: {0}")]
    Peer(#[from] PeerRejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PeerRejection {
    #[error("UID mismatch")]
    Uid,
    #[error("PID unavailable or invalid")]
    Pid,
    #[error("process metadata unavailable")]
    Process,
    #[error("executable flavor mismatch")]
    Flavor,
    #[error("executable mismatch")]
    Executable,
    #[error("process role mismatch")]
    Role,
    #[error("announcement and endpoint PIDs differ")]
    EndpointPid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedClientRole {
    SingletonOwner,
    RestoreChild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    pub uid: u32,
    pub pid: i32,
}

#[derive(Debug, Clone)]
struct PeerProcess {
    uid: u32,
    pid: i32,
    executable: PathBuf,
    args: Vec<Vec<u8>>,
}

/// A bound focus endpoint whose drop cleanup cannot unlink a replacement.
pub struct BoundFocusEndpoint {
    listener: UnixListener,
    path: PathBuf,
    generation: ClientFocusGeneration,
    device: u64,
    inode: u64,
}

impl BoundFocusEndpoint {
    pub fn bind(generation: ClientFocusGeneration) -> Result<Self, FocusTransportError> {
        let path = client_focus_socket_path(generation);
        let parent = path.parent().ok_or(FocusTransportError::InvalidEndpoint)?;
        Self::bind_in(parent, generation)
    }

    fn bind_in(
        parent: &Path,
        generation: ClientFocusGeneration,
    ) -> Result<Self, FocusTransportError> {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        let path = parent.join(generation.socket_name());
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            drop(std::fs::remove_file(&path));
            return Err(error.into());
        }
        let metadata = std::fs::metadata(&path)?;
        Ok(Self { listener, path, generation, device: metadata.dev(), inode: metadata.ino() })
    }

    #[must_use]
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn generation(&self) -> ClientFocusGeneration {
        self.generation
    }
}

impl Drop for BoundFocusEndpoint {
    fn drop(&mut self) {
        let is_original = std::fs::metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode);
        if is_original {
            cleanup_socket(&self.path);
        }
    }
}

/// Result of attempting to become a singleton client process.
pub enum SingletonResult {
    /// We are the singleton. The listener is ready to accept focus commands.
    Primary { listener: UnixListener, socket_path: PathBuf },
    /// Another instance is already running and was told to focus.
    AlreadyRunning,
}

/// Attempt to become the singleton settings process.
///
/// Acquires an advisory flock, then tries to bind the socket. If another
/// instance holds the socket, sends it a focus command and returns
/// `AlreadyRunning`.
pub fn acquire(anchor: Option<SettingsWindowAnchor>) -> Result<SingletonResult, String> {
    acquire_at(&settings_lock_path(), settings_socket_path(), anchor)
}

/// Attempt to become the singleton terminal-client process.
pub fn acquire_terminal() -> Result<SingletonResult, String> {
    let result = acquire_at(&client_lock_path(), client_socket_path(), None)?;
    if matches!(result, SingletonResult::Primary { .. })
        && let Err(error) = spawn_focus_endpoint_cleanup()
    {
        tracing::warn!(%error, "restore-child focus endpoint cleanup did not start");
    }
    Ok(result)
}

/// Core singleton acquisition against explicit lock/socket paths.
///
/// [`acquire`] delegates here with the real
/// [`settings_lock_path`]/[`settings_socket_path`]. Splitting the path
/// resolution out lets the focus-handoff test drive the bind-or-connect
/// sequence against a throwaway temp socket instead of the live per-user
/// runtime socket, so it stays deterministic and never collides with a running
/// client.
pub fn acquire_at(
    lock_path: &Path,
    socket_path: PathBuf,
    anchor: Option<SettingsWindowAnchor>,
) -> Result<SingletonResult, String> {
    // Ensure the parent directory exists with 0o700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create socket dir: {e}"))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("failed to set socket dir permissions: {e}"))?;
    }

    // Acquire advisory flock to serialise the bind-or-connect sequence.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| format!("failed to open lock file: {e}"))?;

    let lock_file = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| format!("flock failed: {e}"))?;

    // The socket owns the singleton lifetime. The flock only serialises this
    // bind-or-connect decision; retaining it would block later launchers before
    // they could send the focus command.
    let result = match try_bind(&socket_path) {
        Ok(listener) => Ok(SingletonResult::Primary { listener, socket_path }),
        Err(_bind_err) => {
            // Socket exists — try to connect and send focus.
            if send_focus_to_existing(&socket_path, anchor) {
                Ok(SingletonResult::AlreadyRunning)
            } else {
                // Stale socket — remove and retry.
                drop(std::fs::remove_file(&socket_path));
                let listener = try_bind(&socket_path)
                    .map_err(|e| format!("failed to bind after stale removal: {e}"))?;
                Ok(SingletonResult::Primary { listener, socket_path })
            }
        }
    };
    drop(lock_file);
    result
}

/// Try to bind the Unix socket. Sets permissions to 0o600.
fn try_bind(socket_path: &std::path::Path) -> Result<UnixListener, std::io::Error> {
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    // Set socket file permissions to 0o600 (defense-in-depth).
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;

    Ok(listener)
}

/// Try to connect to an existing settings process and send focus command.
fn send_focus_to_existing(
    socket_path: &std::path::Path,
    anchor: Option<SettingsWindowAnchor>,
) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    write_command(&mut stream, &SettingsWindowCommand::focus(anchor)).is_ok()
}

/// Write a singleton command as a newline-terminated JSON payload.
pub fn write_command(
    stream: &mut UnixStream,
    command: &SettingsWindowCommand,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(FOCUS_IO_TIMEOUT))?;
    let payload = serde_json::to_vec(command)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    stream.write_all(&payload)?;
    stream.write_all(b"\n")
}

pub fn write_terminal_focus_command(
    stream: &mut UnixStream,
    command: &TerminalFocusCommand,
) -> Result<(), FocusTransportError> {
    write_json_line(stream, command)
}

pub fn read_terminal_focus_command(
    stream: &UnixStream,
) -> Result<TerminalFocusCommand, FocusTransportError> {
    read_json_line(stream)
}

pub fn write_focus_endpoint_request(
    stream: &mut UnixStream,
    request: &FocusEndpointRequest,
) -> Result<(), FocusTransportError> {
    write_json_line(stream, request)
}

pub fn read_focus_endpoint_request(
    stream: &UnixStream,
) -> Result<FocusEndpointRequest, FocusTransportError> {
    read_json_line(stream)
}

pub fn write_focus_endpoint_result(
    stream: &mut UnixStream,
    result: &FocusEndpointResult,
) -> Result<(), FocusTransportError> {
    write_json_line(stream, result)
}

pub fn read_focus_endpoint_result(
    stream: &UnixStream,
) -> Result<FocusEndpointResult, FocusTransportError> {
    read_json_line(stream)
}

fn write_json_line<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), FocusTransportError> {
    write_json_line_with_timeout(stream, value, FOCUS_IO_TIMEOUT)
}

fn write_json_line_with_timeout<T: Serialize>(
    stream: &mut UnixStream,
    value: &T,
    timeout: Duration,
) -> Result<(), FocusTransportError> {
    stream.set_write_timeout(Some(timeout))?;
    let payload = serde_json::to_vec(value).map_err(|_| FocusTransportError::Malformed)?;
    if payload.len() + 1 > MAX_COMMAND_LINE_BYTES {
        return Err(FocusTransportError::TooLarge);
    }
    stream.write_all(&payload).map_err(map_io_error)?;
    stream.write_all(b"\n").map_err(map_io_error)
}

fn read_json_line<T: DeserializeOwned>(stream: &UnixStream) -> Result<T, FocusTransportError> {
    read_json_line_with_timeout(stream, FOCUS_IO_TIMEOUT)
}

fn read_json_line_with_timeout<T: DeserializeOwned>(
    stream: &UnixStream,
    timeout: Duration,
) -> Result<T, FocusTransportError> {
    stream.set_read_timeout(Some(timeout))?;
    let mut reader = std::io::BufReader::new(stream.take((MAX_COMMAND_LINE_BYTES + 1) as u64));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(map_io_error)?;
    if line.len() > MAX_COMMAND_LINE_BYTES {
        return Err(FocusTransportError::TooLarge);
    }
    let Some(payload) = line.strip_suffix(b"\n") else {
        return Err(FocusTransportError::Truncated);
    };
    serde_json::from_slice(payload).map_err(|_| FocusTransportError::Malformed)
}

fn map_io_error(error: std::io::Error) -> FocusTransportError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        FocusTransportError::Timeout
    } else {
        FocusTransportError::Io(error)
    }
}

fn connect_with_timeout(path: &Path, timeout: Duration) -> Result<UnixStream, FocusTransportError> {
    let fd = nix::sys::socket::socket(
        nix::sys::socket::AddressFamily::Unix,
        nix::sys::socket::SockType::Stream,
        nix::sys::socket::SockFlag::SOCK_CLOEXEC | nix::sys::socket::SockFlag::SOCK_NONBLOCK,
        None,
    )
    .map_err(std::io::Error::from)?;
    let address = nix::sys::socket::UnixAddr::new(path).map_err(std::io::Error::from)?;
    let deadline = Instant::now() + timeout;
    loop {
        match nix::sys::socket::connect(fd.as_raw_fd(), &address) {
            Ok(()) | Err(nix::errno::Errno::EISCONN) => {
                let stream = UnixStream::from(fd);
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(
                nix::errno::Errno::EINPROGRESS
                | nix::errno::Errno::EALREADY
                | nix::errno::Errno::EAGAIN,
            ) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(
                nix::errno::Errno::EINPROGRESS
                | nix::errno::Errno::EALREADY
                | nix::errno::Errno::EAGAIN,
            ) => return Err(FocusTransportError::Timeout),
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
    }
}

/// Check if an incoming singleton connection is from the same UID.
///
/// Linux: `SO_PEERCRED` via nix. macOS: `getpeereid()` via nix.
/// Returns `false` if credentials cannot be retrieved or the UID does not match.
pub fn verify_peer_uid(stream: &UnixStream) -> bool {
    let peer_uid = match get_peer_uid(stream) {
        Ok(uid) => uid,
        Err(e) => {
            tracing::warn!("failed to get peer credentials: {e}");
            return false;
        }
    };
    let expected = scribe_common::socket::current_uid();
    if peer_uid != expected {
        tracing::warn!(peer_uid, expected, "rejected singleton connection from different UID");
        return false;
    }
    true
}

/// Linux: use `SO_PEERCRED` via nix `getsockopt`.
#[cfg(target_os = "linux")]
fn get_peer_uid(stream: &UnixStream) -> Result<u32, String> {
    let cred = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .map_err(|e| format!("getsockopt(SO_PEERCRED) failed: {e}"))?;
    Ok(cred.uid())
}

/// macOS: use nix's safe `getpeereid()` wrapper.
#[cfg(not(target_os = "linux"))]
fn get_peer_uid(stream: &UnixStream) -> Result<u32, String> {
    nix::unistd::getpeereid(stream)
        .map(|(uid, _gid)| uid.as_raw())
        .map_err(|e| format!("getpeereid failed: {e}"))
}

/// Verify a connected peer's kernel identity, executable, flavor, and role.
pub fn verify_focus_peer(
    stream: &UnixStream,
    expected_role: ExpectedClientRole,
) -> Result<AuthenticatedPeer, FocusTransportError> {
    let peer = read_peer_process(stream)?;
    verify_peer_process_claim(
        &peer,
        current_uid(),
        current_identity(),
        &allowed_client_executable_paths(),
        expected_role,
    )?;
    Ok(AuthenticatedPeer { uid: peer.uid, pid: peer.pid })
}

fn verify_peer_process_claim(
    peer: &PeerProcess,
    expected_uid: u32,
    expected_identity: AppIdentity,
    allowed_executables: &[PathBuf],
    expected_role: ExpectedClientRole,
) -> Result<(), PeerRejection> {
    if peer.uid != expected_uid {
        return Err(PeerRejection::Uid);
    }
    if peer.pid <= 0 {
        return Err(PeerRejection::Pid);
    }
    if AppIdentity::detect_from_path(&peer.executable) != expected_identity {
        return Err(PeerRejection::Flavor);
    }
    if !allowed_executables.iter().any(|allowed| same_executable_path(&peer.executable, allowed)) {
        return Err(PeerRejection::Executable);
    }
    let is_restore_child = peer
        .args
        .iter()
        .any(|argument| argument == crate::restore_replay::RESTORE_CHILD_ARG.as_bytes());
    let role_matches = match expected_role {
        ExpectedClientRole::SingletonOwner => !is_restore_child,
        ExpectedClientRole::RestoreChild => is_restore_child,
    };
    if !role_matches {
        return Err(PeerRejection::Role);
    }
    Ok(())
}

fn allowed_client_executable_paths() -> Vec<PathBuf> {
    let identity = current_identity();
    let mut paths = vec![PathBuf::from("/usr/bin").join(identity.client_binary_name())];
    if let Ok(current) = std::env::current_exe() {
        paths.push(current.clone());
        if let Some(parent) = current.parent() {
            paths.push(parent.join(identity.client_binary_name()));
        }
    }
    paths
}

fn same_executable_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(target_os = "linux")]
fn read_peer_process(stream: &UnixStream) -> Result<PeerProcess, PeerRejection> {
    let credentials =
        nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
            .map_err(|_| PeerRejection::Process)?;
    let pid = credentials.pid();
    if pid <= 0 {
        return Err(PeerRejection::Pid);
    }
    let executable =
        std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| PeerRejection::Process)?;
    let args = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|_| PeerRejection::Process)?
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Ok(PeerProcess { uid: credentials.uid(), pid, executable, args })
}

#[cfg(target_os = "macos")]
fn read_peer_process(stream: &UnixStream) -> Result<PeerProcess, PeerRejection> {
    let (uid, _gid) = nix::unistd::getpeereid(stream).map_err(|_| PeerRejection::Process)?;
    let pid = nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerPid)
        .map_err(|_| PeerRejection::Pid)?;
    let executable =
        scribe_server::macos_proc::macos_proc_exe_path(pid).ok_or(PeerRejection::Process)?;
    let args = scribe_server::macos_proc::macos_proc_args(pid).ok_or(PeerRejection::Process)?;
    Ok(PeerProcess { uid: uid.as_raw(), pid, executable, args })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_peer_process(_stream: &UnixStream) -> Result<PeerProcess, PeerRejection> {
    Err(PeerRejection::Pid)
}

pub fn validate_activation_announcement(
    command: &TerminalFocusCommand,
) -> Result<(), FocusTransportError> {
    let TerminalFocusCommand::AnnounceActivation { generation, endpoint, .. } = command else {
        return Err(FocusTransportError::UnexpectedCommand);
    };
    if endpoint != &client_focus_socket_path(*generation) {
        return Err(FocusTransportError::InvalidEndpoint);
    }
    let metadata =
        std::fs::symlink_metadata(endpoint).map_err(|_| FocusTransportError::InvalidEndpoint)?;
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(FocusTransportError::InvalidEndpoint);
    }
    Ok(())
}

pub fn validate_focus_endpoint_request(
    request: &FocusEndpointRequest,
    generation: ClientFocusGeneration,
) -> Result<(), FocusTransportError> {
    let matches = match request {
        FocusEndpointRequest::Activate { generation: requested } => *requested == generation,
        FocusEndpointRequest::Probe { socket_tag } => socket_tag == &generation.socket_tag(),
    };
    if matches { Ok(()) } else { Err(FocusTransportError::GenerationMismatch) }
}

fn validate_focus_endpoint_result(
    result: &FocusEndpointResult,
    generation: ClientFocusGeneration,
) -> Result<(), FocusTransportError> {
    let matches = match result {
        FocusEndpointResult::Activated { generation: returned }
        | FocusEndpointResult::Alive { generation: returned } => *returned == generation,
        FocusEndpointResult::Rejected { .. } => true,
    };
    if matches { Ok(()) } else { Err(FocusTransportError::GenerationMismatch) }
}

/// Publish a positive restore-child activation to the singleton owner.
pub fn announce_activation(
    generation: ClientFocusGeneration,
    window_id: WindowId,
) -> Result<(), FocusTransportError> {
    let mut stream = connect_with_timeout(&client_socket_path(), FOCUS_IO_TIMEOUT)?;
    verify_focus_peer(&stream, ExpectedClientRole::SingletonOwner)?;
    write_terminal_focus_command(
        &mut stream,
        &TerminalFocusCommand::AnnounceActivation {
            generation,
            endpoint: client_focus_socket_path(generation),
            window_id,
        },
    )
}

/// Authenticate an announcement and prove its endpoint belongs to its sender.
pub fn authenticate_activation_announcement(
    stream: &UnixStream,
    command: &TerminalFocusCommand,
) -> Result<AuthenticatedPeer, FocusTransportError> {
    validate_activation_announcement(command)?;
    let publisher = verify_focus_peer(stream, ExpectedClientRole::RestoreChild)?;
    let TerminalFocusCommand::AnnounceActivation { generation, endpoint, .. } = command else {
        return Err(FocusTransportError::UnexpectedCommand);
    };
    let endpoint_peer = probe_focus_endpoint(endpoint, *generation)?;
    if endpoint_peer.pid != publisher.pid {
        return Err(PeerRejection::EndpointPid.into());
    }
    Ok(publisher)
}

/// Ask one authenticated restore-child endpoint to activate its recent window.
pub fn request_focus_endpoint(
    generation: ClientFocusGeneration,
) -> Result<FocusEndpointResult, FocusTransportError> {
    let endpoint = client_focus_socket_path(generation);
    let mut stream = connect_with_timeout(&endpoint, FOCUS_IO_TIMEOUT)?;
    verify_focus_peer(&stream, ExpectedClientRole::RestoreChild)?;
    write_focus_endpoint_request(&mut stream, &FocusEndpointRequest::Activate { generation })?;
    let result = read_focus_endpoint_result(&stream)?;
    validate_focus_endpoint_result(&result, generation)?;
    Ok(result)
}

fn probe_focus_endpoint(
    endpoint: &Path,
    generation: ClientFocusGeneration,
) -> Result<AuthenticatedPeer, FocusTransportError> {
    let mut stream = connect_with_timeout(endpoint, FOCUS_IO_TIMEOUT)?;
    let peer = verify_focus_peer(&stream, ExpectedClientRole::RestoreChild)?;
    write_focus_endpoint_request(
        &mut stream,
        &FocusEndpointRequest::Probe { socket_tag: generation.socket_tag() },
    )?;
    let result = read_focus_endpoint_result(&stream)?;
    validate_focus_endpoint_result(&result, generation)?;
    if !matches!(result, FocusEndpointResult::Alive { .. }) {
        return Err(FocusTransportError::UnexpectedCommand);
    }
    Ok(peer)
}

/// Parse a command from a connected client.
///
/// Reads a single newline-terminated JSON line and returns the `cmd` field.
pub fn read_command(stream: &UnixStream) -> Option<SettingsWindowCommand> {
    // Set a short read timeout to avoid blocking the GTK loop.
    drop(stream.set_read_timeout(Some(std::time::Duration::from_millis(100))));

    let mut reader = std::io::BufReader::new(stream.take((MAX_COMMAND_LINE_BYTES + 1) as u64));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return None;
    }
    if line.len() > MAX_COMMAND_LINE_BYTES {
        tracing::warn!("singleton command exceeded size limit");
        return None;
    }

    serde_json::from_str::<SettingsWindowCommand>(line.trim()).ok()
}

/// Cleanup: remove the socket file.
pub fn cleanup_socket(socket_path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(socket_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %socket_path.display(), "failed to remove singleton socket: {e}");
    }
}

/// Start one bounded cleanup pass without delaying client startup.
pub fn spawn_focus_endpoint_cleanup() -> Result<(), String> {
    let Some(runtime_dir) = client_socket_path().parent().map(Path::to_path_buf) else {
        return Err("client socket has no runtime directory".to_owned());
    };
    std::thread::Builder::new()
        .name("scribe-focus-cleanup".to_owned())
        .spawn(move || {
            let stats = cleanup_focus_endpoints_in(&runtime_dir, probe_cleanup_endpoint);
            tracing::debug!(
                scanned = stats.scanned,
                removed = stats.removed,
                "restore-child focus endpoint cleanup finished"
            );
        })
        .map(|_| ())
        .map_err(|error| format!("failed to start focus endpoint cleanup: {error}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointLiveness {
    Live,
    Dead,
    Indeterminate,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CleanupStats {
    scanned: usize,
    removed: usize,
}

fn cleanup_focus_endpoints_in(
    runtime_dir: &Path,
    mut probe: impl FnMut(&Path, &str, Duration) -> EndpointLiveness,
) -> CleanupStats {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return CleanupStats::default();
    };
    let mut stats = CleanupStats::default();
    for entry in entries.flatten() {
        if stats.scanned == CLEANUP_LIMIT || Instant::now() >= deadline {
            break;
        }
        let path = entry.path();
        let Some(socket_tag) = focus_socket_tag(&path) else {
            continue;
        };
        stats.scanned += 1;
        let Ok(before) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !before.file_type().is_socket() {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if probe(&path, &socket_tag, remaining) != EndpointLiveness::Dead {
            continue;
        }
        let is_same_socket = std::fs::symlink_metadata(&path).is_ok_and(|after| {
            after.file_type().is_socket()
                && after.dev() == before.dev()
                && after.ino() == before.ino()
        });
        if is_same_socket && std::fs::remove_file(&path).is_ok() {
            stats.removed += 1;
        }
    }
    stats
}

fn focus_socket_tag(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_str()?;
    let tag = filename.strip_prefix(CLIENT_FOCUS_SOCKET_PREFIX)?.strip_suffix(".sock")?;
    if tag.len() == 16
        && tag.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Some(tag.to_owned())
    } else {
        None
    }
}

fn probe_cleanup_endpoint(path: &Path, socket_tag: &str, remaining: Duration) -> EndpointLiveness {
    let timeout = remaining.min(FOCUS_IO_TIMEOUT);
    let mut stream = match connect_with_timeout(path, timeout) {
        Ok(stream) => stream,
        Err(FocusTransportError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return EndpointLiveness::Dead;
        }
        Err(_) => return EndpointLiveness::Indeterminate,
    };
    if verify_focus_peer(&stream, ExpectedClientRole::RestoreChild).is_err() {
        return EndpointLiveness::Indeterminate;
    }
    let request = FocusEndpointRequest::Probe { socket_tag: socket_tag.to_owned() };
    if write_json_line_with_timeout(&mut stream, &request, timeout).is_err() {
        return EndpointLiveness::Indeterminate;
    }
    match read_json_line_with_timeout::<FocusEndpointResult>(&stream, timeout) {
        Ok(FocusEndpointResult::Alive { generation }) if generation.socket_tag() == socket_tag => {
            EndpointLiveness::Live
        }
        _ => EndpointLiveness::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::app::AppIdentity;
    use scribe_common::ids::WindowId;
    use scribe_common::socket::ClientFocusGeneration;

    fn generation(value: &str) -> ClientFocusGeneration {
        value.parse().unwrap()
    }

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "scribe-focus-{label}-{}-{}",
            std::process::id(),
            ClientFocusGeneration::new().socket_tag()
        ))
    }

    /// The singleton focus-handoff contract: the first `acquire_at` becomes the
    /// `Primary` and binds the socket; a second `acquire_at` against the same
    /// paths finds the live socket, sends a `focus` command carrying the launch
    /// anchor, and returns `AlreadyRunning`. The primary then accepts the
    /// connection, verifies the peer UID, and reads back exactly that focus
    /// command — proving the second launch hands focus to the running window
    /// instead of opening a duplicate.
    // @lat: [[test#Test Harness#Terminal Client Singleton#Duplicate launch sends focus without waiting]]
    #[test]
    fn focus_handoff_routes_second_launch_to_primary() {
        let dir = std::env::temp_dir()
            .join(format!("scribe-gpui-settings-singleton-{}", std::process::id()));
        drop(std::fs::create_dir_all(&dir));
        let lock_path = dir.join("settings.lock");
        let socket_path = dir.join("settings.sock");
        drop(std::fs::remove_file(&socket_path));

        let anchor = SettingsWindowAnchor { x: 12, y: 34, width: 800, height: 600 };

        let primary = acquire_at(&lock_path, socket_path.clone(), None)
            .expect("first acquire should become primary");
        let listener = match primary {
            SingletonResult::Primary { listener, .. } => listener,
            SingletonResult::AlreadyRunning => panic!("first acquire must be primary"),
        };

        // A second launch must detect the live socket and hand off focus.
        let second_lock = lock_path.clone();
        let second_socket = socket_path.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let second_thread = std::thread::spawn(move || {
            let result = acquire_at(&second_lock, second_socket, Some(anchor));
            drop(result_tx.send(result));
        });
        let second = result_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second acquire must not wait for the primary to exit")
            .expect("second acquire should succeed");
        assert!(
            matches!(second, SingletonResult::AlreadyRunning),
            "second acquire must return AlreadyRunning after sending focus"
        );

        // The primary accepts the handoff connection and reads the focus command.
        // The listener is non-blocking; spin briefly for the pending connection.
        let mut accepted = None;
        for _ in 0..100 {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted = Some(stream);
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
        let stream = accepted.expect("primary should accept the handoff connection");
        assert!(verify_peer_uid(&stream), "handoff peer must be the same UID");

        let command = read_command(&stream).expect("primary should read a focus command");
        assert_eq!(command.cmd, "focus", "handoff command must be a focus request");
        let received = command.anchor.expect("focus command must carry the launch anchor");
        assert_eq!(received.x, anchor.x);
        assert_eq!(received.y, anchor.y);
        assert_eq!(received.width, anchor.width);
        assert_eq!(received.height, anchor.height);

        cleanup_socket(&socket_path);
        second_thread.join().expect("second acquire thread should exit");
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Dead owner socket is reclaimed]]
    #[test]
    fn stale_owner_socket_is_reclaimed() {
        let dir = std::env::temp_dir()
            .join(format!("scribe-client-singleton-stale-{}", std::process::id()));
        drop(std::fs::create_dir_all(&dir));
        let lock_path = dir.join("client.lock");
        let socket_path = dir.join("client.sock");
        drop(std::fs::remove_file(&socket_path));
        drop(UnixListener::bind(&socket_path).expect("stale owner should bind once"));

        let result = acquire_at(&lock_path, socket_path.clone(), None)
            .expect("a dead owner's socket should be reclaimed");
        assert!(matches!(result, SingletonResult::Primary { .. }));

        cleanup_socket(&socket_path);
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Legacy focus frame stays byte compatible]]
    #[test]
    fn legacy_focus_frame_stays_byte_compatible() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        write_command(&mut writer, &SettingsWindowCommand::focus(None)).unwrap();
        writer.shutdown(std::net::Shutdown::Write).unwrap();

        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"{\"cmd\":\"focus\",\"anchor\":null}\n");
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Typed focus transport frames round trip]]
    #[test]
    fn typed_focus_transport_frames_round_trip() {
        let generation = generation("12345678-90ab-cdef-1234-567890abcdef");
        let endpoint = scribe_common::socket::client_focus_socket_path(generation);
        let window_id = WindowId::new();
        let command = TerminalFocusCommand::AnnounceActivation {
            generation,
            endpoint: endpoint.clone(),
            window_id,
        };
        let (mut announcement_writer, announcement_reader) = UnixStream::pair().unwrap();
        write_terminal_focus_command(&mut announcement_writer, &command).unwrap();
        let TerminalFocusCommand::AnnounceActivation {
            generation: decoded_generation,
            endpoint: decoded_endpoint,
            window_id: decoded_window,
        } = read_terminal_focus_command(&announcement_reader).unwrap()
        else {
            panic!("announcement must retain its command kind");
        };
        assert_eq!(decoded_generation, generation);
        assert_eq!(decoded_endpoint, endpoint);
        assert_eq!(decoded_window, window_id);

        let (mut request_writer, request_reader) = UnixStream::pair().unwrap();
        let request = FocusEndpointRequest::Activate { generation };
        write_focus_endpoint_request(&mut request_writer, &request).unwrap();
        assert_eq!(read_focus_endpoint_request(&request_reader).unwrap(), request);

        let (mut result_writer, result_reader) = UnixStream::pair().unwrap();
        let result = FocusEndpointResult::Activated { generation };
        write_focus_endpoint_result(&mut result_writer, &result).unwrap();
        assert_eq!(read_focus_endpoint_result(&result_reader).unwrap(), result);
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Malformed focus transport is rejected]]
    #[test]
    fn malformed_truncated_unknown_and_oversized_frames_are_rejected() {
        for bytes in [
            b"not-json\n".as_slice(),
            b"{\"cmd\":\"announce_activation\"}".as_slice(),
            b"{\"cmd\":\"future_command\"}\n".as_slice(),
        ] {
            let (mut writer, reader) = UnixStream::pair().unwrap();
            writer.write_all(bytes).unwrap();
            writer.shutdown(std::net::Shutdown::Write).unwrap();
            assert!(read_terminal_focus_command(&reader).is_err());
        }

        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(&vec![b'x'; MAX_COMMAND_LINE_BYTES + 1]).unwrap();
        writer.write_all(b"\n").unwrap();
        assert!(matches!(read_terminal_focus_command(&reader), Err(FocusTransportError::TooLarge)));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Peer claims fail closed]]
    #[test]
    fn peer_claims_fail_closed_on_every_identity_boundary() {
        let expected_exe = PathBuf::from("/usr/bin/scribe-client");
        let valid = PeerProcess {
            uid: 1000,
            pid: 42,
            executable: expected_exe.clone(),
            args: vec![b"scribe-client".to_vec(), b"--restore-child".to_vec()],
        };
        let verify = |peer: &PeerProcess| {
            verify_peer_process_claim(
                peer,
                1000,
                AppIdentity::stable(),
                std::slice::from_ref(&expected_exe),
                ExpectedClientRole::RestoreChild,
            )
        };
        assert_eq!(verify(&valid), Ok(()));

        let mut wrong_uid = valid.clone();
        wrong_uid.uid = 1001;
        assert_eq!(verify(&wrong_uid), Err(PeerRejection::Uid));
        let mut invalid_pid = valid.clone();
        invalid_pid.pid = 0;
        assert_eq!(verify(&invalid_pid), Err(PeerRejection::Pid));
        let mut wrong_executable = valid.clone();
        wrong_executable.executable = PathBuf::from("/tmp/scribe-client");
        assert_eq!(verify(&wrong_executable), Err(PeerRejection::Executable));
        let mut wrong_flavor = valid.clone();
        wrong_flavor.executable = PathBuf::from("/usr/bin/scribe-dev");
        assert_eq!(verify(&wrong_flavor), Err(PeerRejection::Flavor));
        let mut wrong_role = valid;
        wrong_role.args.pop();
        assert_eq!(verify(&wrong_role), Err(PeerRejection::Role));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Endpoint path and generation fail closed]]
    #[test]
    fn endpoint_path_and_generation_fail_closed() {
        let first = generation("12345678-90ab-cdef-1234-567890abcdef");
        let second = generation("abcdef12-3456-7890-abcd-ef1234567890");
        let command = TerminalFocusCommand::AnnounceActivation {
            generation: first,
            endpoint: scribe_common::socket::client_focus_socket_path(second),
            window_id: WindowId::new(),
        };
        assert!(matches!(
            validate_activation_announcement(&command),
            Err(FocusTransportError::InvalidEndpoint)
        ));
        assert!(matches!(
            validate_focus_endpoint_request(
                &FocusEndpointRequest::Activate { generation: second },
                first,
            ),
            Err(FocusTransportError::GenerationMismatch)
        ));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Focus endpoint permissions are private]]
    #[test]
    fn focus_endpoint_parent_and_socket_permissions_are_private() {
        let dir = test_dir("permissions");
        let generation = ClientFocusGeneration::new();
        let endpoint = BoundFocusEndpoint::bind_in(&dir, generation).unwrap();

        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(endpoint.path()).unwrap().permissions().mode() & 0o777, 0o600);

        drop(endpoint);
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Focus endpoint reads time out]]
    #[test]
    fn focus_endpoint_reads_time_out_at_the_transport_bound() {
        let (_writer, reader) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(read_focus_endpoint_result(&reader), Err(FocusTransportError::Timeout)));
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Crash debris cleanup is bounded and conservative]]
    #[test]
    fn crash_debris_cleanup_removes_dead_and_preserves_live_endpoints() {
        let dir = test_dir("cleanup");
        std::fs::create_dir_all(&dir).unwrap();
        let dead_generation = ClientFocusGeneration::new();
        let dead_path = dir.join(dead_generation.socket_name());
        drop(UnixListener::bind(&dead_path).unwrap());
        let live_generation = ClientFocusGeneration::new();
        let live_path = dir.join(live_generation.socket_name());
        let _live = UnixListener::bind(&live_path).unwrap();
        let unrelated = dir.join("server.sock");
        drop(UnixListener::bind(&unrelated).unwrap());

        let stats = cleanup_focus_endpoints_in(&dir, |path, _, _| {
            if path == live_path { EndpointLiveness::Live } else { EndpointLiveness::Dead }
        });

        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.removed, 1);
        assert!(!dead_path.exists());
        assert!(live_path.exists());
        assert!(unrelated.exists());
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Orderly cleanup preserves a replacement generation]]
    #[test]
    fn orderly_cleanup_does_not_unlink_a_replacement_socket() {
        let dir = test_dir("replacement");
        let generation = ClientFocusGeneration::new();
        let old = BoundFocusEndpoint::bind_in(&dir, generation).unwrap();
        let path = old.path().to_path_buf();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();

        drop(old);
        assert!(path.exists());

        drop(replacement);
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[test#Test Harness#Terminal Client Singleton#Legacy acquisition ignores focus endpoint debris]]
    #[test]
    fn legacy_acquisition_ignores_focus_endpoint_debris() {
        let dir = test_dir("rollback");
        std::fs::create_dir_all(&dir).unwrap();
        let focus_path = dir.join(ClientFocusGeneration::new().socket_name());
        let _focus = UnixListener::bind(&focus_path).unwrap();

        let singleton =
            acquire_at(&dir.join("client.lock"), dir.join("client.sock"), None).unwrap();
        assert!(matches!(singleton, SingletonResult::Primary { .. }));
        assert!(focus_path.exists());

        cleanup_socket(&dir.join("client.sock"));
        drop(std::fs::remove_dir_all(&dir));
    }
}
