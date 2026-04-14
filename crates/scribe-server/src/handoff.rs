//! Hot-reload handoff protocol for zero-downtime server upgrades.
//!
//! Two roles participate in a handoff:
//!
//! - **Sender** (old server): serialises session and workspace state, then
//!   transmits all PTY master file descriptors via `SCM_RIGHTS` ancillary data.
//! - **Receiver** (new server launched with `--upgrade`): connects to the
//!   handoff socket, receives the state + fds, and reconstructs sessions.
//!
//! The handoff socket path is platform-specific (see `scribe_common::socket`).

use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

use nix::sys::socket::{self, AddressFamily, Backlog, MsgFlags, SockFlag, SockType, UnixAddr};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use unix_ancillary::{AncillaryData, SocketAncillary};

use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::error::ScribeError;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::SessionContext;
use scribe_common::screen::ScreenSnapshot;
use scribe_common::socket::{current_uid, handoff_socket_path};

pub use crate::workspace_manager::HandoffWindowState;

use crate::ipc_server::LiveSessionRegistry;
use crate::workspace_manager::WorkspaceManager;

// ── Wire types ──────────────────────────────────────────────────────

/// Current handoff protocol version. Bump when the serialised format changes.
///
/// A version mismatch causes the new server to abort the handoff and perform
/// a full restart instead, so all live sessions are terminated.
const HANDOFF_VERSION: u32 = 4;

/// Magic bytes the receiver sends to request an upgrade.
const UPGRADE_REQUEST: &[u8] = b"SCRIBE_UPGRADE";

/// Magic bytes the receiver sends after successful fd reception.
const ACK: &[u8] = b"ACK";

/// Maximum serialised state size we accept (1 GiB). Prevents a rogue peer
/// from making us allocate unbounded memory.
const MAX_STATE_SIZE: u32 = 1024 * 1024 * 1024;

/// Maximum number of PTY fds we support in a single handoff.
const MAX_FDS: usize = 1024;

/// Complete serialised server state for a handoff.
#[derive(Serialize, Deserialize)]
pub struct HandoffState {
    pub version: u32,
    pub sessions: Vec<HandoffSession>,
    pub workspaces: Vec<HandoffWorkspace>,
    /// Legacy single workspace tree — used as fallback when no per-window
    /// trees exist.
    pub workspace_tree: Option<scribe_common::protocol::WorkspaceTreeNode>,
    /// Per-window state: which sessions belong to which window, and each
    /// window's workspace tree.
    #[serde(default)]
    pub windows: Vec<HandoffWindowState>,
}

/// Per-session state transferred during handoff.
#[derive(Serialize, Deserialize)]
pub struct HandoffSession {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub child_pid: u32,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
    pub snapshot: Option<ScreenSnapshot>,
    /// Last-known terminal title. `#[serde(default)]` for backward compat with
    /// old servers that did not include this field.
    #[serde(default)]
    pub title: Option<String>,
    /// Last-known session shell name. `#[serde(default)]` for backward compat.
    #[serde(default = "default_shell_name")]
    pub shell_name: String,
    /// Last-known Codex task label. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub codex_task_label: Option<String>,
    /// Last-known working directory. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Last-known remote/tmux context. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub context: Option<SessionContext>,
    /// Last-known AI process state. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub ai_state: Option<AiProcessState>,
    /// Launch-time AI provider hint. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub ai_provider_hint: Option<AiProvider>,
}

fn default_shell_name() -> String {
    String::from("shell")
}

/// Per-workspace state transferred during handoff.
#[derive(Serialize, Deserialize)]
pub struct HandoffWorkspace {
    pub id: WorkspaceId,
    pub name: Option<String>,
    pub accent_color: String,
    pub session_ids: Vec<SessionId>,
    /// Direction of the split that created this workspace.
    pub split_direction: Option<scribe_common::protocol::LayoutDirection>,
    /// Absolute path to the project directory (root + first CWD component).
    #[serde(default)]
    pub project_root: Option<PathBuf>,
}

struct HandoffPayload {
    state_bytes: Vec<u8>,
    fds: Vec<Arc<OwnedFd>>,
}

// ── Sender (old server) ─────────────────────────────────────────────

/// Listen for an incoming upgrade connection and perform the handoff.
///
/// This function blocks (async) until a new server connects and the handoff
/// completes. On success the caller should exit so the new server takes over.
pub async fn run_handoff_listener(
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
) -> Result<(), ScribeError> {
    let path = handoff_socket_path();
    let listen_async = prepare_handoff_listener(&path)?;
    wait_for_successful_handoff(&listen_async, &path, &live_sessions, &workspace_manager).await
}

async fn wait_for_successful_handoff(
    listen_async: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    path: &PathBuf,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) -> Result<(), ScribeError> {
    // Loop so the old server survives a failed handoff (e.g. version
    // mismatch) and keeps serving until a compatible upgrade arrives or
    // postinst cold-restarts via systemctl.
    loop {
        let peer_fd = accept_handoff_peer(listen_async).await?;
        if process_handoff_peer(&peer_fd, path, live_sessions, workspace_manager).await {
            return Ok(());
        }
    }
}

fn prepare_handoff_listener(
    path: &PathBuf,
) -> Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, ScribeError> {
    // Prepare the socket directory and clean stale socket.
    prepare_handoff_socket(path)?;

    let listen_fd = socket::socket(AddressFamily::Unix, SockType::Stream, cloexec_flag(), None)
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff socket() failed: {e}") })?;
    set_cloexec_if_needed(&listen_fd)?;

    let addr = UnixAddr::new(path).map_err(|e| ScribeError::IpcError {
        reason: format!("handoff UnixAddr::new failed: {e}"),
    })?;

    socket::bind(listen_fd.as_raw_fd(), &addr)
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff bind failed: {e}") })?;

    // Restrict the socket file to owner-only access (0600). The parent
    // directory is already 0700, but defense-in-depth against umask variance.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| ScribeError::Io { source: e })?;

    let backlog = Backlog::new(1).map_err(|e| ScribeError::IpcError {
        reason: format!("handoff Backlog::new failed: {e}"),
    })?;

    socket::listen(&listen_fd, backlog)
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff listen failed: {e}") })?;

    info!(?path, "handoff listener ready");

    tokio::io::unix::AsyncFd::new(listen_fd).map_err(|e| ScribeError::Io { source: e })
}

async fn accept_handoff_peer(
    listen_async: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
) -> Result<std::os::fd::OwnedFd, ScribeError> {
    loop {
        let mut guard = listen_async.readable().await.map_err(|e| ScribeError::Io { source: e })?;

        match rustix::net::accept(listen_async.get_ref()) {
            Ok(peer_fd) => break Ok(peer_fd),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                guard.clear_ready();
            }
            Err(e) => {
                break Err(ScribeError::IpcError { reason: format!("handoff accept failed: {e}") });
            }
        }
    }
}

async fn process_handoff_peer(
    peer_fd: &std::os::fd::OwnedFd,
    path: &PathBuf,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) -> bool {
    if let Err(e) = receive_upgrade_request(peer_fd) {
        warn!("handoff upgrade request failed: {e}");
        return false;
    }

    let payload = match prepare_handoff_payload(live_sessions, workspace_manager).await {
        Ok(payload) => payload,
        Err(e) => {
            warn!("handoff serialization failed: {e}");
            return false;
        }
    };

    if let Err(e) = send_handoff_payload(peer_fd, &payload) {
        warn!("handoff transfer failed: {e}");
        return false;
    }

    if let Err(e) = receive_handoff_ack(peer_fd.as_raw_fd()) {
        warn!("handoff not acknowledged (version mismatch?): {e}");
        return false;
    }

    cleanup_handoff_socket(path);
    true
}

fn receive_upgrade_request(peer_fd: &OwnedFd) -> Result<(), ScribeError> {
    verify_peer_uid(peer_fd)?;
    read_upgrade_request(peer_fd.as_raw_fd())?;
    info!("received upgrade request from new server");
    Ok(())
}

async fn prepare_handoff_payload(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) -> Result<HandoffPayload, ScribeError> {
    let (state, fds) = serialize_state(live_sessions, workspace_manager).await;
    let state_bytes = rmp_serde::to_vec(&state).map_err(ScribeError::from)?;
    Ok(HandoffPayload { state_bytes, fds })
}

fn send_handoff_payload(peer_fd: &OwnedFd, payload: &HandoffPayload) -> Result<(), ScribeError> {
    send_state_bytes(peer_fd.as_raw_fd(), &payload.state_bytes)?;
    info!(
        state_len = payload.state_bytes.len(),
        fd_count = payload.fds.len(),
        "sent handoff state"
    );

    if payload.fds.is_empty() {
        return Ok(());
    }

    send_fds(peer_fd, &payload.fds)?;
    info!(count = payload.fds.len(), "sent PTY fds via SCM_RIGHTS");
    Ok(())
}

fn receive_handoff_ack(raw_peer: RawFd) -> Result<(), ScribeError> {
    read_ack(raw_peer)?;
    info!("received ACK from new server — handoff complete");
    Ok(())
}

fn cleanup_handoff_socket(path: &PathBuf) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(?path, "failed to remove handoff socket: {e}");
        }
    }
}

/// Prepare the handoff socket path: create parent dirs, remove stale socket.
fn prepare_handoff_socket(path: &PathBuf) -> Result<(), ScribeError> {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(ScribeError::Io { source: e });
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    Ok(())
}

/// Verify the peer's UID matches our own.
///
/// Linux: `SO_PEERCRED` via `getsockopt`.
/// macOS: `getpeereid()` via nix.
fn verify_peer_uid(fd: &OwnedFd) -> Result<(), ScribeError> {
    let peer_uid = get_peer_uid(fd)?;
    let expected = current_uid();
    if peer_uid != expected {
        return Err(ScribeError::IpcError {
            reason: format!("handoff peer UID mismatch: got {peer_uid}, expected {expected}"),
        });
    }

    debug!(uid = expected, "handoff peer UID verified");
    Ok(())
}

/// Linux: use `SO_PEERCRED` via nix `getsockopt`.
#[cfg(target_os = "linux")]
fn get_peer_uid(fd: &OwnedFd) -> Result<u32, ScribeError> {
    let cred = socket::getsockopt(fd, socket::sockopt::PeerCredentials).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff getsockopt(SO_PEERCRED) failed: {e}") }
    })?;
    Ok(cred.uid())
}

/// macOS: use nix's safe `getpeereid()` wrapper.
#[cfg(not(target_os = "linux"))]
fn get_peer_uid(fd: &OwnedFd) -> Result<u32, ScribeError> {
    nix::unistd::getpeereid(fd)
        .map(|(uid, _gid)| uid.as_raw())
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff getpeereid failed: {e}") })
}

/// Read the upgrade request magic bytes from the peer.
fn read_upgrade_request(fd: RawFd) -> Result<(), ScribeError> {
    let mut buf = [0u8; 32];

    let bytes_read = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = socket::recvmsg::<()>(fd, &mut iov, None, MsgFlags::empty()).map_err(|e| {
            ScribeError::IpcError {
                reason: format!("handoff recvmsg (upgrade request) failed: {e}"),
            }
        })?;
        msg.bytes
    };

    let received = buf.get(..bytes_read).ok_or_else(|| ScribeError::IpcError {
        reason: "upgrade request bytes out of range".to_owned(),
    })?;

    if received != UPGRADE_REQUEST {
        return Err(ScribeError::IpcError { reason: "invalid upgrade request magic".to_owned() });
    }

    Ok(())
}

/// Collect serialisable state from the live session registry and workspace manager.
async fn serialize_state(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) -> (HandoffState, Vec<Arc<OwnedFd>>) {
    let (sessions, fds) = crate::ipc_server::serialize_live_for_handoff(live_sessions).await;
    let (workspaces, workspace_tree, windows) =
        workspace_manager.read().await.serialize_for_handoff();

    let state =
        HandoffState { version: HANDOFF_VERSION, sessions, workspaces, workspace_tree, windows };

    (state, fds)
}

/// Send length-prefixed serialised state bytes over the socket.
fn send_state_bytes(fd: RawFd, state_bytes: &[u8]) -> Result<(), ScribeError> {
    // Send length as u32 big-endian.
    let len: u32 = state_bytes.len().try_into().map_err(|_| ScribeError::IpcError {
        reason: "handoff state too large to encode as u32 length prefix".to_owned(),
    })?;
    let len_bytes = len.to_be_bytes();

    let iov = [IoSlice::new(&len_bytes), IoSlice::new(state_bytes)];

    socket::sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff sendmsg (state) failed: {e}") }
    })?;

    Ok(())
}

/// Send file descriptors via `SCM_RIGHTS`.
fn send_fds(fd: &OwnedFd, fds: &[Arc<OwnedFd>]) -> Result<(), ScribeError> {
    let borrowed: Vec<_> = fds.iter().map(|owned_fd| owned_fd.as_fd()).collect();
    let mut ancillary_buf = vec![0u8; SocketAncillary::buffer_size_for_rights(borrowed.len())];
    let mut ancillary = SocketAncillary::new(&mut ancillary_buf);
    ancillary.add_fds(&borrowed).map_err(|e| ScribeError::IpcError {
        reason: format!("handoff ancillary buffer setup failed: {e}"),
    })?;

    let iov = [IoSlice::new(b"fds")];
    unix_ancillary::cmsg_sendmsg(fd.as_fd(), &iov, &ancillary).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff sendmsg (SCM_RIGHTS) failed: {e}") }
    })?;

    Ok(())
}

/// Read the ACK magic bytes from the peer.
fn read_ack(fd: RawFd) -> Result<(), ScribeError> {
    let mut buf = [0u8; 8];

    let bytes_read = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = socket::recvmsg::<()>(fd, &mut iov, None, MsgFlags::empty()).map_err(|e| {
            ScribeError::IpcError { reason: format!("handoff recvmsg (ACK) failed: {e}") }
        })?;
        msg.bytes
    };

    let received = buf
        .get(..bytes_read)
        .ok_or_else(|| ScribeError::IpcError { reason: "ACK bytes out of range".to_owned() })?;

    if received != ACK {
        return Err(ScribeError::IpcError { reason: "invalid ACK from new server".to_owned() });
    }

    Ok(())
}

// ── Receiver (new server with --upgrade) ────────────────────────────

/// Connect to the old server's handoff socket and receive state + fds.
///
/// Returns the deserialised state and the received PTY master fds (in the
/// same order as `state.sessions`).
pub fn receive_handoff() -> Result<(HandoffState, Vec<OwnedFd>), ScribeError> {
    let path = handoff_socket_path();

    let sock_fd = socket::socket(AddressFamily::Unix, SockType::Stream, cloexec_flag(), None)
        .map_err(|e| ScribeError::IpcError {
            reason: format!("handoff receiver socket() failed: {e}"),
        })?;
    set_cloexec_if_needed(&sock_fd)?;

    let addr = UnixAddr::new(&path).map_err(|e| ScribeError::IpcError {
        reason: format!("handoff receiver UnixAddr::new failed: {e}"),
    })?;

    socket::connect(sock_fd.as_raw_fd(), &addr).map_err(|e| ScribeError::IpcError {
        reason: format!("handoff connect to {} failed: {e}", path.display()),
    })?;

    info!(?path, "connected to old server for handoff");

    let fd = sock_fd.as_raw_fd();

    // Send upgrade request.
    send_upgrade_request(fd)?;

    // Read state (length-prefixed).  A deserialization failure most likely
    // means the old server uses a different HandoffState layout (field
    // count changed between versions).  Surface this as a version mismatch
    // so the postinst script can offer a cold restart.
    let state = match read_state(fd) {
        Ok(s) => s,
        Err(ScribeError::Deserialization { .. }) => {
            return Err(ScribeError::IpcError {
                reason: format!(
                    "handoff version mismatch: incompatible state format \
                     (expected version {HANDOFF_VERSION})"
                ),
            });
        }
        Err(e) => return Err(e),
    };

    if state.version != HANDOFF_VERSION {
        return Err(ScribeError::IpcError {
            reason: format!(
                "handoff version mismatch: got {}, expected {HANDOFF_VERSION}",
                state.version
            ),
        });
    }

    info!(
        version = state.version,
        sessions = state.sessions.len(),
        workspaces = state.workspaces.len(),
        "received handoff state"
    );

    // Receive session PTY fds via SCM_RIGHTS.
    let total_fds = state.sessions.len();
    let fds = if total_fds == 0 { Vec::new() } else { receive_fds(&sock_fd, total_fds)? };

    info!(count = fds.len(), "received PTY fds via SCM_RIGHTS");

    // Send ACK.
    send_ack(fd)?;

    Ok((state, fds))
}

/// Send the upgrade request magic bytes.
fn send_upgrade_request(fd: RawFd) -> Result<(), ScribeError> {
    let iov = [IoSlice::new(UPGRADE_REQUEST)];
    socket::sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff sendmsg (upgrade request) failed: {e}") }
    })?;
    Ok(())
}

/// Send the ACK magic bytes.
fn send_ack(fd: RawFd) -> Result<(), ScribeError> {
    let iov = [IoSlice::new(ACK)];
    socket::sendmsg::<()>(fd, &iov, &[], MsgFlags::empty(), None).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff sendmsg (ACK) failed: {e}") }
    })?;
    Ok(())
}

/// Read length-prefixed serialised state from the socket.
fn read_state(fd: RawFd) -> Result<HandoffState, ScribeError> {
    let state_len = read_state_length(fd)?;

    if state_len > MAX_STATE_SIZE {
        return Err(ScribeError::IpcError {
            reason: format!("handoff state too large: {state_len} bytes (max {MAX_STATE_SIZE})"),
        });
    }

    let state_buf = read_exact_bytes(fd, state_len as usize)?;
    rmp_serde::from_slice(&state_buf).map_err(ScribeError::from)
}

/// Read the 4-byte big-endian state length prefix.
fn read_state_length(fd: RawFd) -> Result<u32, ScribeError> {
    let mut len_buf = [0u8; 4];
    let mut iov = [IoSliceMut::new(&mut len_buf)];

    let msg = socket::recvmsg::<()>(fd, &mut iov, None, MsgFlags::MSG_WAITALL).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff recvmsg (state length) failed: {e}") }
    })?;

    if msg.bytes != 4 {
        return Err(ScribeError::IpcError {
            reason: format!("handoff state length: expected 4 bytes, got {}", msg.bytes),
        });
    }

    Ok(u32::from_be_bytes(len_buf))
}

/// Read exactly `len` bytes from the socket, looping on partial reads.
fn read_exact_bytes(fd: RawFd, len: usize) -> Result<Vec<u8>, ScribeError> {
    let mut buf = vec![0u8; len];
    let mut total_read = 0usize;

    while total_read < buf.len() {
        let remaining = buf.get_mut(total_read..).ok_or_else(|| ScribeError::IpcError {
            reason: "state buffer slice out of range".to_owned(),
        })?;
        let mut iov = [IoSliceMut::new(remaining)];

        let msg = socket::recvmsg::<()>(fd, &mut iov, None, MsgFlags::empty()).map_err(|e| {
            ScribeError::IpcError { reason: format!("handoff recvmsg (state data) failed: {e}") }
        })?;

        if msg.bytes == 0 {
            return Err(ScribeError::IpcError {
                reason: "handoff peer closed connection while reading state".to_owned(),
            });
        }

        total_read = total_read.checked_add(msg.bytes).ok_or_else(|| ScribeError::IpcError {
            reason: "handoff state read byte count overflowed".to_owned(),
        })?;
    }

    Ok(buf)
}

/// Receive file descriptors from `SCM_RIGHTS` ancillary data.
fn receive_fds(fd: &OwnedFd, expected_count: usize) -> Result<Vec<OwnedFd>, ScribeError> {
    if expected_count > MAX_FDS {
        return Err(ScribeError::IpcError {
            reason: format!("too many fds to receive: {expected_count} (max {MAX_FDS})"),
        });
    }

    let mut data_buf = [0u8; 8];
    let mut ancillary_buf = vec![0u8; SocketAncillary::buffer_size_for_rights(expected_count)];
    let mut ancillary = SocketAncillary::new(&mut ancillary_buf);
    let mut iov = [IoSliceMut::new(&mut data_buf)];

    let bytes_read =
        unix_ancillary::cmsg_recvmsg(fd.as_fd(), &mut iov, &mut ancillary).map_err(|e| {
            ScribeError::IpcError { reason: format!("handoff recvmsg (SCM_RIGHTS) failed: {e}") }
        })?;

    if bytes_read == 0 {
        return Err(ScribeError::IpcError {
            reason: "handoff peer closed connection while reading PTY fds".to_owned(),
        });
    }

    if ancillary.is_truncated() {
        return Err(ScribeError::IpcError {
            reason: "handoff ancillary data was truncated while receiving PTY fds".to_owned(),
        });
    }

    let mut received_fds = Vec::with_capacity(expected_count);
    for message in ancillary.messages() {
        match message {
            AncillaryData::ScmRights(rights) => received_fds.extend(rights),
        }
    }

    if received_fds.len() != expected_count {
        return Err(ScribeError::IpcError {
            reason: format!(
                "fd count mismatch: expected {expected_count}, got {}",
                received_fds.len()
            ),
        });
    }

    Ok(received_fds)
}

/// On Linux, `SOCK_CLOEXEC` is available as a socket flag.
#[cfg(target_os = "linux")]
fn cloexec_flag() -> SockFlag {
    SockFlag::SOCK_CLOEXEC
}

/// On macOS (and other non-Linux), `SOCK_CLOEXEC` does not exist.
/// Return empty flags; the caller must use `set_cloexec_if_needed`.
#[cfg(not(target_os = "linux"))]
fn cloexec_flag() -> SockFlag {
    SockFlag::empty()
}

/// Ensure the socket fd has `FD_CLOEXEC` set after creation.
fn set_cloexec_if_needed(fd: &OwnedFd) -> Result<(), ScribeError> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};

    let current = fcntl(fd, FcntlArg::F_GETFD)
        .map_err(|e| ScribeError::IpcError { reason: format!("fcntl(F_GETFD) failed: {e}") })?;

    let mut flags = FdFlag::from_bits_truncate(current);
    flags.insert(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(flags)).map_err(|e| ScribeError::IpcError {
        reason: format!("fcntl(F_SETFD, FD_CLOEXEC) failed: {e}"),
    })?;

    Ok(())
}

// ── Permissions helper ──────────────────────────────────────────────

use std::os::unix::fs::PermissionsExt as _;
