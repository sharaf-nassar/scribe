//! Hot-reload handoff protocol for zero-downtime server upgrades.
//!
//! Two roles participate in a handoff:
//!
//! - **Sender** (old server): serialises session and workspace state, then
//!   transmits all PTY master file descriptors via `SCM_RIGHTS` ancillary data.
//! - **Receiver** (new server launched with `--upgrade`): connects to the
//!   handoff socket, receives the state + fds, and reconstructs sessions.
//!
//! Feature 013 (remote window control) carries NO remote-control state across a
//! handoff — neither the listener's enabled/bind flag nor any active
//! remote-connection metadata. The receiver re-derives the `[remote]` listener
//! purely from on-disk config: `run_normal_server` and `run_upgrade_receiver`
//! share `run_server_loop`, which spawns the same `remote_supervisor` in both
//! paths, so only the enabled-state survives an upgrade (via TOML, never the
//! wire). The old server's remote TCP connections drop when its process exits,
//! and the remote client auto-reconnects to the rebound listener (research D6).
//! `HandoffState` and `HANDOFF_VERSION` are therefore unchanged by that feature
//! (see `specs/013-remote-window-control/contracts/remote-protocol.md`,
//! Compatibility statement).
//!
//! Feature 014 (LAN remote window control) extends the same rule to its larger
//! surface: the per-transport LAN listener, the per-install device identity, and
//! the trusted-device / trusted-network stores all re-derive from config plus
//! on-disk state after a handoff — never from the wire. The receiver's
//! `remote_supervisor` reconciles the `[remote.lan]` transport from config and
//! re-materializes the device identity via `LanRuntime::ensure_identity`, whose
//! `load_or_generate` reads the keyring-sealed private key and on-disk cert, so
//! the reconstituted server presents the SAME pinned identity and a reconnecting
//! client's TLS pin still matches. `RemoteControl::new` reloads both trust stores
//! from disk (`TrustedDevicesStore::load` / `TrustedNetworksStore::load`), so an
//! already-approved device stays approved. The old server's live LAN TLS
//! connections drop when its process exits and the client auto-reconnects exactly
//! as on the tailnet path. The device keypair lives on disk/keyring, so nothing
//! LAN-related need cross the wire and `HANDOFF_VERSION` stays unchanged (see
//! `specs/014-lan-remote-control/contracts/lan-protocol.md`, Compatibility
//! statement).
//!
//! The handoff socket path is platform-specific (see `scribe_common::socket`).

use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::sys::socket::{self, AddressFamily, Backlog, MsgFlags, SockFlag, SockType, UnixAddr};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use unix_ancillary::{AncillaryData, SocketAncillary};

use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::app::current_identity;
use scribe_common::error::ScribeError;
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{SessionContext, ShellTool};
use scribe_common::screen::ScreenSnapshot;
use scribe_common::screen_replay::SessionReplay;
use scribe_common::socket::{current_uid, handoff_socket_path, server_socket_path};

pub use crate::workspace_manager::HandoffWindowState;

use crate::ipc_server::LiveSessionRegistry;
use crate::workspace_manager::WorkspaceManager;
use crate::workspace_transfer::{TransferGate, TransferLedgerEntry};

// ── Wire types ──────────────────────────────────────────────────────

/// Current handoff protocol version. Bump when the serialised format changes.
///
/// v6 switched the wire from positional (`rmp_serde::to_vec`, `MessagePack`
/// array) to named (`rmp_serde::to_vec_named`, `MessagePack` map). With named
/// encoding, additive `#[serde(default)]` fields survive across versions
/// regardless of position — pre-v6 the wire was positional, so any field
/// insertion in the middle of `HandoffState`/`HandoffSession` silently
/// misaligned every later field even when the new field had `#[serde(default)]`
/// (see commit history for `session_replay` and `task_label`, both of which
/// were inserted mid-struct without breaking the version number).
///
/// Hot-reload from a pre-v6 sender is best-effort: `rmp_serde::from_slice`
/// can decode either an array or a map, but the pre-v6 struct shape may not
/// match the current one. When deserialization fails the error is propagated
/// verbatim (no more "version mismatch" masking) and the scribe-client
/// `wait_for_refreshed_server` path on macOS detects the stuck old server and
/// asks the user to approve a cold restart instead of terminating it.
///
/// Features 013 (tailnet) and 014 (LAN) remote window control added no fields
/// to the handoff shape — the remote/LAN listener state, the per-install device
/// identity, and the trusted-device/-network stores are all re-derived from
/// config and on-disk state by the receiver rather than carried on the wire (see
/// the module docs and [`HandoffState`]) — so this stays at v6.
///
/// Spec 017 US7-2 added [`HandoffSession::child_identity`], which also stays at
/// v6: named encoding fills a missing `#[serde(default)]` field from either
/// direction, and the receiver treats an absent identity as "unproven" rather
/// than as a decode failure. Bump ONLY when the serialised shape changes in a
/// way `#[serde(default)]` cannot absorb.
///
/// Spec 020 added [`HandoffSession::image_state`], which is additive in the
/// same `#[serde(default)]` sense — but only in one direction. A v6 server
/// silently ignores it, so image-bearing payloads declare v7 while image-free
/// payloads remain v6.
///
/// Pi provider state adds an enum value an older receiver cannot deserialize.
/// Any payload carrying [`AiProvider::Pi`] therefore declares v8. A v8 receiver
/// still accepts v6 and v7 senders for forward upgrades, while v6/v7 receivers
/// refuse v8 before acknowledging so the current server keeps running.
pub const HANDOFF_VERSION: u32 = 8;

/// Version a payload carrying terminal image state declares.
const HANDOFF_VERSION_WITH_IMAGES: u32 = 7;

/// Version an image-free, pre-Pi payload declares.
const HANDOFF_VERSION_WITHOUT_IMAGES: u32 = 6;

/// Version a payload must declare for the sessions it actually carries.
///
/// For pre-Pi state, turning the master image switch off makes the next
/// upgrade payload v6 again, so an older server accepts it.
// @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
#[must_use]
pub fn handoff_state_version(sessions: &[HandoffSession]) -> u32 {
    if sessions.iter().any(session_carries_pi_provider) {
        HANDOFF_VERSION
    } else if sessions.iter().any(|session| session.image_state.is_some()) {
        HANDOFF_VERSION_WITH_IMAGES
    } else {
        HANDOFF_VERSION_WITHOUT_IMAGES
    }
}

fn session_carries_pi_provider(session: &HandoffSession) -> bool {
    session.ai_state.as_ref().map(|state| state.provider) == Some(AiProvider::Pi)
        || session.ai_provider_hint == Some(AiProvider::Pi)
}

/// Whether a receiver supporting `supported` accepts a payload at `version`.
///
/// Pre-Pi receivers keep their N/N-1 contract. The v8 receiver additionally
/// accepts v6 because an old sender deliberately emits v6 for image-free state.
/// Newer payloads are always refused, preserving safe downgrade behavior.
// @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
#[must_use]
pub const fn handoff_version_accepted(version: u32, supported: u32) -> bool {
    version == supported
        || version == supported.saturating_sub(1)
        || (supported == HANDOFF_VERSION && version == HANDOFF_VERSION_WITHOUT_IMAGES)
}

/// Magic bytes the receiver sends to request an upgrade.
const UPGRADE_REQUEST: &[u8] = b"SCRIBE_UPGRADE";

/// Command-line flag required on handoff receivers.
const UPGRADE_ARG: &str = "--upgrade";

/// Magic bytes the receiver sends after successful fd reception.
const ACK: &[u8] = b"ACK";

/// Maximum serialised state size we accept (256 MiB). Prevents a rogue peer
/// from making us allocate unbounded memory.
const MAX_STATE_SIZE: u32 = 256 * 1024 * 1024;

/// Maximum number of PTY fds we support in a single handoff.
const MAX_FDS: usize = 1024;

/// Complete serialised server state for a handoff.
///
/// Features 013 (tailnet) and 014 (LAN) remote window control intentionally add
/// nothing here: no remote/LAN listener enabled/bind flag, no active
/// remote-connection metadata, no device identity, and no trust-store contents.
/// The receiver re-derives the `[remote]` and `[remote.lan]` listeners purely
/// from config via the shared `remote_supervisor` startup, reloads the device
/// identity from the keyring/disk and the trusted-device/-network stores from
/// disk, and dropped remote/LAN connections recover through the client's
/// auto-reconnect loop (contracts Compatibility statements; research D6).
/// Carrying any of that state would force a `HANDOFF_VERSION` bump for no
/// benefit.
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
    /// Active GitHub CI windows contain no credential and re-poll on takeover.
    #[serde(default)]
    pub ci_windows: Vec<crate::github_ci::HandoffCiWindow>,
    /// Bounded workspace-transfer result ledger (spec 029), so a transfer ACK
    /// lost across an upgrade still deduplicates the client's retry. Additive
    /// `#[serde(default)]` — an older peer simply starts with an empty ledger.
    #[serde(default)]
    pub transfer_ledger: Vec<TransferLedgerEntry>,
}

/// Per-session state transferred during handoff.
#[derive(Serialize, Deserialize)]
pub struct HandoffSession {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub child_pid: u32,
    /// Per-boot identity token for `child_pid`, captured when the child was
    /// spawned (spec 017 US7-2). The receiver re-reads it before hanging the
    /// child up at close time, so a recycled PID is never signalled.
    ///
    /// `#[serde(default)]` — absent on payloads from senders that predate the
    /// field, and `None` on platforms that cannot report a start time. Either
    /// way the successor cannot prove the PID is still this child, so it skips
    /// the close-time SIGHUP and logs instead of signalling blind.
    #[serde(default)]
    pub child_identity: Option<crate::child_identity::ChildIdentity>,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
    pub snapshot: Option<ScreenSnapshot>,
    /// v5 replay payload (zstd-compressed ANSI produced by `snapshot_to_ansi`).
    /// v5+ senders populate this and leave `snapshot` at None. v4 senders leave
    /// this at None and populate `snapshot`. Receivers prefer `session_replay`
    /// when present and fall through to `snapshot` otherwise.
    #[serde(default)]
    pub session_replay: Option<SessionReplay>,
    /// Last-known terminal title. `#[serde(default)]` for backward compat with
    /// old servers that did not include this field.
    #[serde(default)]
    pub title: Option<String>,
    /// Last-known OSC 0/1 icon/tab title.
    #[serde(default)]
    pub icon_title: Option<String>,
    /// Last-known session shell name. `#[serde(default)]` for backward compat.
    #[serde(default = "default_shell_name")]
    pub shell_name: String,
    /// Last-known provider task label. `#[serde(default)]` for backward compat.
    #[serde(default)]
    pub task_label: Option<String>,
    /// Legacy Codex task label. `#[serde(default)]` for backward compat.
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
    /// Launch-only tool identity. `#[serde(default)]` keeps older handoffs
    /// compatible; AI metadata remains authoritative if both are present.
    #[serde(default)]
    pub shell_tool: Option<ShellTool>,
    /// Prompt history for the running conversation, carried so a server
    /// upgrade leaves every AI pane's prompt bar intact. `#[serde(default)]`
    /// for backward compat: a sender that predates the field just means the
    /// first prompt after the upgrade rebuilds the history.
    #[serde(default)]
    pub prompt_state: Option<scribe_common::protocol::SessionPromptState>,
    /// Stable owner and envelope coordinates used by env persistence cleanup.
    /// Older senders omit both, so restore falls back to workspace membership
    /// for the window and keeps the previous no-envelope behavior.
    #[serde(default)]
    pub env_window_id: Option<WindowId>,
    #[serde(default)]
    pub env_envelope_id: Option<String>,
    /// Committed image scene, paused framing, and any in-flight chunked
    /// transfer (spec 020). `#[serde(default)]` so a pre-image sender restores
    /// as a session with an empty scene rather than failing to decode.
    ///
    /// Present only while the master image switch is on; its presence is what
    /// lifts the payload to v7 and blocks a rollback that would drop it.
    ///
    /// `skip_serializing_if` matters as much as `default`: an image-free
    /// payload must omit this key, or an old receiver would silently discard
    /// image state while accepting the payload as compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_state: Option<crate::terminal_image_handoff::SessionImageHandoff>,
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

#[derive(Clone, Copy)]
struct HandoffSources<'a> {
    live_sessions: &'a LiveSessionRegistry,
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
    github_ci_tracker: &'a crate::github_ci::GithubCiTrackerHandle,
    workspace_transfers: &'a TransferGate,
}

#[derive(Debug, Clone, Copy)]
struct PeerIdentity {
    uid: u32,
    pid: Option<i32>,
}

// ── Sender (old server) ─────────────────────────────────────────────

/// Listen for an incoming upgrade connection and perform the handoff.
///
/// This function blocks (async) until a new server connects and the handoff
/// completes. On success the caller should exit so the new server takes over.
pub async fn run_handoff_listener(
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    github_ci_tracker: crate::github_ci::GithubCiTrackerHandle,
    workspace_transfers: TransferGate,
) -> Result<(), ScribeError> {
    let path = handoff_socket_path();
    let listen_async = prepare_handoff_listener(&path)?;
    wait_for_successful_handoff(
        &listen_async,
        &path,
        HandoffSources {
            live_sessions: &live_sessions,
            workspace_manager: &workspace_manager,
            github_ci_tracker: &github_ci_tracker,
            workspace_transfers: &workspace_transfers,
        },
    )
    .await
}

async fn wait_for_successful_handoff(
    listen_async: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    path: &PathBuf,
    sources: HandoffSources<'_>,
) -> Result<(), ScribeError> {
    // Loop so the old server survives a failed handoff (e.g. version
    // mismatch) and keeps serving until a compatible upgrade arrives or
    // postinst cold-restarts via systemctl.
    loop {
        let peer_fd = accept_handoff_peer(listen_async).await?;
        let done = process_handoff_peer(&peer_fd, path, sources).await;
        if done {
            return Ok(());
        }
        // Failed handoff: this server keeps serving, so transfers may run
        // again — the state snapshotted above is stale and discarded.
        sources.workspace_transfers.lock().await.abort_handoff();
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
    sources: HandoffSources<'_>,
) -> bool {
    if let Err(e) = receive_upgrade_request(peer_fd) {
        warn!("handoff upgrade request failed: {e}");
        return false;
    }

    // Latch the transfer refusal BEFORE the snapshot: a transfer committed
    // after serialization would mutate state the payload no longer carries,
    // so from here until this handoff succeeds (process exit) or fails (the
    // caller clears the latch) every transfer gets `HandoffInProgress`.
    sources.workspace_transfers.lock().await.begin_handoff();

    let payload = match prepare_handoff_payload(sources).await {
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
    verify_peer_identity(peer_fd)?;
    read_upgrade_request(peer_fd.as_raw_fd())?;
    info!("received upgrade request from new server");
    Ok(())
}

async fn prepare_handoff_payload(
    sources: HandoffSources<'_>,
) -> Result<HandoffPayload, ScribeError> {
    let (state, fds) = serialize_state(
        sources.live_sessions,
        sources.workspace_manager,
        sources.github_ci_tracker,
        sources.workspace_transfers,
    )
    .await;
    // Named-map encoding (since v6) so additive `#[serde(default)]` fields on
    // the receiver are tolerated regardless of insertion position. Positional
    // `to_vec` would force append-only discipline that the codebase has not
    // historically respected.
    let state_bytes = rmp_serde::to_vec_named(&state).map_err(ScribeError::from)?;
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
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(?path, "failed to remove handoff socket: {e}");
    }
}

/// Prepare the handoff socket path: create parent dirs, remove stale socket.
fn prepare_handoff_socket(path: &PathBuf) -> Result<(), ScribeError> {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(ScribeError::Io { source: e });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    Ok(())
}

/// Verify the peer is the same UID and, where the platform exposes the peer
/// PID safely, that it is the installed Scribe server running in upgrade mode.
///
/// Linux: `SO_PEERCRED` via `getsockopt`.
/// macOS: `getpeereid()` plus `LOCAL_PEERPID` via nix.
fn verify_peer_identity(fd: &OwnedFd) -> Result<(), ScribeError> {
    let peer = get_peer_identity(fd)?;
    let expected = current_uid();
    if peer.uid != expected {
        return Err(ScribeError::IpcError {
            reason: format!("handoff peer UID mismatch: got {}, expected {expected}", peer.uid),
        });
    }

    verify_peer_process(&peer)?;

    debug!(uid = expected, pid = peer.pid, "handoff peer identity verified");
    Ok(())
}

/// Linux: use `SO_PEERCRED` via nix `getsockopt`.
#[cfg(target_os = "linux")]
fn get_peer_identity(fd: &OwnedFd) -> Result<PeerIdentity, ScribeError> {
    let cred = socket::getsockopt(fd, socket::sockopt::PeerCredentials).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff getsockopt(SO_PEERCRED) failed: {e}") }
    })?;
    Ok(PeerIdentity { uid: cred.uid(), pid: Some(cred.pid()) })
}

/// macOS: use nix's safe `getpeereid()` wrapper.
#[cfg(target_os = "macos")]
fn get_peer_identity(fd: &OwnedFd) -> Result<PeerIdentity, ScribeError> {
    let (uid, _gid) = nix::unistd::getpeereid(fd)
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff getpeereid failed: {e}") })?;
    let pid = socket::getsockopt(fd, socket::sockopt::LocalPeerPid).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff getsockopt(LOCAL_PEERPID) failed: {e}") }
    })?;
    Ok(PeerIdentity { uid: uid.as_raw(), pid: Some(pid) })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn get_peer_identity(fd: &OwnedFd) -> Result<PeerIdentity, ScribeError> {
    nix::unistd::getpeereid(fd)
        .map(|(uid, _gid)| PeerIdentity { uid: uid.as_raw(), pid: None })
        .map_err(|e| ScribeError::IpcError { reason: format!("handoff getpeereid failed: {e}") })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verify_peer_process(peer: &PeerIdentity) -> Result<(), ScribeError> {
    let Some(pid) = peer.pid else {
        return Err(ScribeError::IpcError { reason: "handoff peer PID unavailable".to_owned() });
    };
    if pid <= 0 {
        return Err(ScribeError::IpcError { reason: format!("handoff peer PID invalid: {pid}") });
    }

    verify_peer_cmdline(pid)?;
    verify_peer_executable(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn verify_peer_process(_peer: &PeerIdentity) -> Result<(), ScribeError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer_cmdline(pid: i32) -> Result<(), ScribeError> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff could not read peer cmdline: {e}") }
    })?;
    let has_upgrade_arg = cmdline.split(|byte| *byte == 0).any(|arg| arg == UPGRADE_ARG.as_bytes());
    if !has_upgrade_arg {
        return Err(ScribeError::IpcError {
            reason: "handoff peer is not running in --upgrade mode".to_owned(),
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_peer_cmdline(pid: i32) -> Result<(), ScribeError> {
    let args = crate::macos_proc::macos_proc_args(pid).ok_or_else(|| ScribeError::IpcError {
        reason: "handoff could not read peer cmdline".to_owned(),
    })?;
    let has_upgrade_arg = args.iter().any(|arg| arg == UPGRADE_ARG.as_bytes());
    if !has_upgrade_arg {
        return Err(ScribeError::IpcError {
            reason: "handoff peer is not running in --upgrade mode".to_owned(),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer_executable(pid: i32) -> Result<(), ScribeError> {
    let peer_exe = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|e| {
        ScribeError::IpcError { reason: format!("handoff could not read peer executable: {e}") }
    })?;
    let allowed = allowed_server_executable_paths();
    if allowed.iter().any(|path| same_executable_path(&peer_exe, path)) {
        return Ok(());
    }

    Err(ScribeError::IpcError {
        reason: format!(
            "handoff peer executable {} is not an allowed server binary",
            peer_exe.display()
        ),
    })
}

#[cfg(target_os = "macos")]
fn verify_peer_executable(pid: i32) -> Result<(), ScribeError> {
    let peer_exe = crate::macos_proc::macos_proc_exe_path(pid).ok_or_else(|| {
        ScribeError::IpcError { reason: "handoff could not read peer executable".to_owned() }
    })?;
    let allowed = allowed_server_executable_paths();
    if allowed.iter().any(|path| same_executable_path(&peer_exe, path)) {
        return Ok(());
    }

    Err(ScribeError::IpcError {
        reason: format!(
            "handoff peer executable {} is not an allowed server binary",
            peer_exe.display()
        ),
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn allowed_server_executable_paths() -> Vec<PathBuf> {
    let identity = current_identity();
    let mut paths = vec![PathBuf::from("/usr/bin").join(identity.server_binary_name())];
    if let Ok(current) = std::env::current_exe() {
        paths.push(current.clone());
        if let Some(parent) = current.parent() {
            paths.push(parent.join(identity.server_binary_name()));
        }
    }
    paths
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn same_executable_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
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
///
/// `pub(crate)` because the crash-recovery dump ([`crate::state_dump`]) reuses
/// this exact collection — dropping the fds, which only a live `SCM_RIGHTS`
/// transfer can carry — so the on-disk dump and the handoff wire can never
/// drift apart in what they capture.
pub async fn serialize_state(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    github_ci_tracker: &crate::github_ci::GithubCiTrackerHandle,
    workspace_transfers: &TransferGate,
) -> (HandoffState, Vec<Arc<OwnedFd>>) {
    // Hold the transfer gate across the whole capture (spec 029 C4): a
    // transfer transaction mutates the live-session and workspace registries
    // under separate guards, so a snapshot taken between them would carry a
    // half-moved workspace. Under the gate this snapshot is strictly pre- or
    // post-transfer state, and the ledger it serializes matches.
    let transfers = workspace_transfers.lock().await;
    let (sessions, fds) = crate::ipc_server::serialize_live_for_handoff(live_sessions).await;
    let (workspaces, workspace_tree, windows) =
        workspace_manager.read().await.serialize_for_handoff();

    let version = handoff_state_version(&sessions);
    let state = HandoffState {
        version,
        sessions,
        workspaces,
        workspace_tree,
        windows,
        ci_windows: github_ci_tracker.handoff_windows(),
        transfer_ledger: transfers.entries(),
    };

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

/// State and resources transferred from the old server during a handoff.
pub type ReceivedHandoff =
    (HandoffState, Vec<OwnedFd>, crate::ipc_server::ServerLock, tokio::net::UnixListener);

/// Connect to the old server's handoff socket and receive state + fds.
///
/// Returns the deserialised state, the received PTY master fds (in the same
/// order as `state.sessions`), and the IPC listener this receiver took over
/// from the old server — see the ACK ordering note below for why the socket is
/// claimed here rather than after the caller has rebuilt its sessions.
pub fn receive_handoff() -> Result<ReceivedHandoff, ScribeError> {
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

    // Read state (length-prefixed).  Deserialization errors usually mean the
    // old server's HandoffState layout differs from ours (e.g. fields inserted
    // mid-struct, an enum variant removed). Propagate the underlying rmp_serde
    // error verbatim so the client log identifies the failing field/type and
    // future incidents are not misdiagnosed as a generic "version mismatch".
    let state = read_state(fd)?;

    // Accept v6-v8 so both image-free and image-bearing pre-Pi senders can
    // upgrade forward. Cross-encoding pre-v6 payloads generally fail decoding
    // first; newer payloads fail this gate before the receiver acknowledges.
    if !handoff_version_accepted(state.version, HANDOFF_VERSION) {
        return Err(ScribeError::IpcError {
            reason: format!(
                "handoff version unsupported: got {}, supported \
                 {HANDOFF_VERSION_WITHOUT_IMAGES}..={HANDOFF_VERSION} \
                 (cold-restart required)",
                state.version,
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

    // Take over the IPC socket BEFORE acknowledging, because the ACK is what
    // tells the old server to exit. Claiming it afterwards — once the caller
    // has rebuilt every session — leaves the path pointing at a dead server for
    // as long as restoration takes, which is proportional to session count. The
    // client polls its lost connection every `SERVER_RETRY_INTERVAL` (100 ms)
    // and cold-starts a stateless server on the first refusal, so a large
    // enough session set made that race a guaranteed loss of the whole handoff.
    //
    // Failing here aborts with the ACK unsent: `wait_for_successful_handoff`
    // loops back and the old server keeps serving every session, instead of
    // exiting into a takeover that never completed.
    //
    // ponytail: bind and ACK cannot be made one atomic step, so a receiver
    // killed between them leaves the old server alive and serving its existing
    // connections on a path that now names this dead inode. Reaching it needs
    // abrupt receiver death inside a two-syscall window; the alternative
    // ordering (ACK, then rename) needs only ordinary preemption to strand the
    // path, which is the far likelier failure. Closing it outright means
    // teaching the old server to rebind and swap the listener inside
    // `run_server_loop`'s select — worth doing only if this is ever observed.
    let (_lock, listener) = crate::ipc_server::acquire_server_socket(&server_socket_path(), true)?;

    // Send ACK.
    send_ack(fd)?;

    // The old process releases its advisory lock only after observing the ACK
    // and defusing the handed-off PTYs. Keep the replacement out of its accept
    // loop until it owns that lock itself, so every post-upgrade server remains
    // protected by the same singleton contract as a cold start.
    let lock = Some(crate::ipc_server::acquire_server_lock()?);

    Ok((state, fds, lock, listener))
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
