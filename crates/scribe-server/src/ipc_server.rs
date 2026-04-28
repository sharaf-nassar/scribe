use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use scribe_pty::codex_hook_log_filter::CodexHookLogFilter;
use scribe_pty::ed3_filter::Ed3Filter;
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::UnixListener;
use tokio::net::unix::UCred;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};
use vte::Parser as VteParser;
use vte::ansi::Processor as AnsiProcessor;

use alacritty_terminal::grid::Dimensions as _;
#[cfg(test)]
use scribe_common::ai_state::AiProcessState;
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::config as scribe_config;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    AutomationAction, ClientMessage, SearchMatch, ServerMessage, SessionInfo, TerminalSize,
    WindowInfo, WorkspaceListEntry, WorkspaceTreeNode,
};
use scribe_common::screen::{ScreenCell, ScreenSnapshot};
use scribe_common::socket::current_uid;
use scribe_pty::event_listener::SessionEvent;
use scribe_pty::metadata::MetadataEvent;
use scribe_pty::osc_interceptor::OscInterceptor;

use crate::handoff::HandoffSession;
use crate::session_manager::{
    ManagedSession, SessionLaunchRequest, SessionManager, build_term_config, snapshot_term,
};
use crate::updater::UpdaterHandle;
use crate::workspace_manager::WorkspaceManager;

/// Buffer size for PTY reads. 64 KiB balances throughput and latency.
const PTY_READ_BUF_SIZE: usize = 64 * 1024;

/// Maximum payload size for a single `KeyInput` message. Legitimate keyboard
/// input is never more than a few dozen bytes; pastes are chunked by the client
/// to fit this limit. Capping at 4 KiB prevents a rogue client from writing
/// 16 MiB (the frame limit) to the PTY in one shot.
const MAX_KEY_INPUT_BYTES: usize = 4 * 1024;

/// Maximum simultaneous IPC client connections. Prevents a same-UID attacker
/// from exhausting memory/tasks by opening thousands of connections.
const MAX_CONNECTIONS: usize = 32;

/// Maximum number of session IDs in a single `Subscribe` message. Prevents
/// a client from holding the workspace write-lock in a tight loop.
const MAX_SUBSCRIBE_IDS: usize = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PreservedAiScrollback {
    baseline_history: Option<usize>,
}

impl PreservedAiScrollback {
    fn reset(&mut self) {
        self.baseline_history = None;
    }

    fn trim_target(&mut self, current_history: usize) -> Option<usize> {
        if let Some(baseline) = self.baseline_history {
            (current_history > baseline).then_some(baseline)
        } else {
            self.baseline_history = Some(current_history);
            None
        }
    }
}

/// Shared writer half of the client connection.
pub type SharedWriter = Arc<Mutex<WriteHalf<tokio::net::UnixStream>>>;

/// Optional client writer: `Some` when a client is attached, `None` when
/// the session is detached (client disconnected). The PTY reader task
/// silently skips sends when `None`.
pub type ClientWriter = Arc<Mutex<Option<SharedWriter>>>;

/// Session IDs currently attached to a specific client connection.
pub type AttachedSessionIds = Arc<Mutex<HashSet<SessionId>>>;

/// Shared pointer from a live session to the current attached-session set for
/// its active client, if any.
pub type SessionAttachment = Arc<Mutex<Option<AttachedSessionIds>>>;

/// Server-wide registry of all running sessions. Shared across client
/// handlers and the handoff listener — sessions survive client disconnects.
pub type LiveSessionRegistry = Arc<RwLock<HashMap<SessionId, LiveSession>>>;

/// Registry of connected client windows, keyed by `WindowId`.
/// Used to broadcast `QuitRequested` to all connected clients.
pub type ConnectedClients = Arc<RwLock<HashMap<WindowId, SharedWriter>>>;

#[derive(Clone)]
pub struct IpcServerState {
    pub session_manager: Arc<SessionManager>,
    pub workspace_manager: Arc<RwLock<WorkspaceManager>>,
    pub live_sessions: LiveSessionRegistry,
    pub connected_clients: ConnectedClients,
    pub updater_handle: Arc<UpdaterHandle>,
}

struct ClientDispatchContext<'a> {
    server: &'a IpcServerState,
    writer: &'a SharedWriter,
    attached_ids: &'a AttachedSessionIds,
    window_id: WindowId,
}

struct CreateSessionRequest {
    workspace_id: WorkspaceId,
    split_direction: Option<scribe_common::protocol::LayoutDirection>,
    cwd: Option<std::path::PathBuf>,
    size: Option<TerminalSize>,
    command: Option<Vec<String>>,
}

#[derive(Clone, Copy)]
struct SessionRuntimeContext<'a> {
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
    live_sessions: &'a LiveSessionRegistry,
}

#[derive(Clone, Copy)]
struct InitialAttachment<'a> {
    writer: Option<&'a SharedWriter>,
    attached_ids: Option<&'a AttachedSessionIds>,
}

/// State needed by the PTY reader task, extracted from `ManagedSession`.
struct PtyReaderState {
    session_id: SessionId,
    child_pid: u32,
    pty_read: ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    clipboard: SharedClipboard,
    /// Reusable buffer for OSC events — cleared between iterations to avoid
    /// allocating a new `Vec` on every PTY read.
    osc_events: Vec<MetadataEvent>,
    /// Last known CWD from `/proc/pid/cwd`, used to detect changes triggered
    /// by title-change events (for shells that emit OSC 0 but not OSC 7).
    last_proc_cwd: Option<std::path::PathBuf>,
    /// Strips ED 3 (`\x1b[3J`) from supported AI sessions to preserve scrollback.
    ed3_filter: Ed3Filter,
    /// Suppresses contiguous Codex hook log blocks when enabled.
    codex_hook_log_filter: CodexHookLogFilter,
    /// Last AI provider seen for this session, if any.
    ai_provider: Option<AiProvider>,
    /// Latest known terminal cell size in pixels for winsize replies.
    cell_width: u16,
    cell_height: u16,
    /// Shared runtime flag updated by `ConfigReloaded`.
    hide_codex_hook_logs: Arc<AtomicBool>,
    /// When `true`, suppress `CSI 3 J` in AI sessions to preserve scrollback.
    preserve_ai_scrollback: Arc<AtomicBool>,
    /// Shared scrollback limit for trimming duplicate AI redraw history.
    scrollback_lines: Arc<AtomicUsize>,
    /// Preserved pre-AI scrollback baseline for this session.
    preserved_ai_scrollback: PreservedAiScrollback,
}

/// A running session in the server-wide registry. Lives independently of
/// any client connection — the `client_writer` is set/cleared as clients
/// attach and detach.
pub struct LiveSession {
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    resize_fd: Arc<OwnedFd>,
    pub(crate) term:
        Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    child_pid: u32,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    workspace_id: WorkspaceId,
    shell_name: String,
    /// Last-known terminal title (OSC 0/2), persisted for reconnect.
    title: String,
    /// Last-known Codex task label, persisted separately from OSC 0/2 titles.
    codex_task_label: Option<String>,
    /// Last-known working directory (OSC 7), persisted for reconnect.
    cwd: Option<std::path::PathBuf>,
    /// Last-known remote/tmux context reported by shell integration.
    context: Option<scribe_common::protocol::SessionContext>,
    /// Last-known AI process state (OSC 1337), persisted for reconnect.
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
    /// Launch-time AI provider hint used when the session CLI does not emit
    /// explicit provider metadata.
    ai_provider_hint: Option<AiProvider>,
    /// Latest known terminal cell size in pixels.
    cell_width: u16,
    cell_height: u16,
    /// Keep the Pty alive so the child process isn't killed by SIGHUP on Drop.
    /// `None` for sessions restored from a hot-reload handoff. Taken and leaked
    /// by `defuse_for_handoff` during hot-reload to prevent SIGHUP.
    pty: Option<alacritty_terminal::tty::Pty>,
    /// Screen snapshot from a hot-reload handoff, sent to the first client
    /// that attaches. Taken (cleared) after first use.
    pub(crate) handoff_snapshot: Option<scribe_common::screen::ScreenSnapshot>,
    /// Shared runtime flag updated by config reloads.
    hide_codex_hook_logs: Arc<AtomicBool>,
    /// Shared runtime flag updated by config reloads.
    preserve_ai_scrollback: Arc<AtomicBool>,
    /// Shared runtime scrollback limit updated by config reloads.
    scrollback_lines: Arc<AtomicUsize>,
}

pub struct AttachSessionData {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub shell_name: String,
    pub client_writer: ClientWriter,
    pub attachment: SessionAttachment,
    pub term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    pub resize_fd: Arc<OwnedFd>,
    pub target_dims: Option<TerminalSize>,
    pub has_handoff_snapshot: bool,
}

impl LiveSession {
    pub fn prepare_attach_data(
        &mut self,
        session_id: SessionId,
        target_dims: Option<TerminalSize>,
    ) -> AttachSessionData {
        if let Some(size) = target_dims.filter(|size| size.has_pixels()) {
            self.cell_width = size.cell_width;
            self.cell_height = size.cell_height;
        }

        AttachSessionData {
            session_id,
            workspace_id: self.workspace_id,
            shell_name: self.shell_name.clone(),
            client_writer: Arc::clone(&self.client_writer),
            attachment: Arc::clone(&self.attachment),
            term: Arc::clone(&self.term),
            resize_fd: Arc::clone(&self.resize_fd),
            target_dims,
            has_handoff_snapshot: self.handoff_snapshot.is_some(),
        }
    }

    pub fn take_handoff_snapshot(&mut self) -> Option<ScreenSnapshot> {
        self.handoff_snapshot.take()
    }
}

type SharedClipboard = Arc<Mutex<ServerClipboard>>;

#[derive(Default)]
struct ServerClipboard {
    clipboard: String,
    selection: String,
}

impl ServerClipboard {
    fn store(&mut self, kind: alacritty_terminal::term::ClipboardType, text: String) {
        match kind {
            alacritty_terminal::term::ClipboardType::Clipboard => self.clipboard = text,
            alacritty_terminal::term::ClipboardType::Selection => self.selection = text,
        }
    }

    fn load(&self, kind: alacritty_terminal::term::ClipboardType) -> &str {
        match kind {
            alacritty_terminal::term::ClipboardType::Clipboard => &self.clipboard,
            alacritty_terminal::term::ClipboardType::Selection => &self.selection,
        }
    }
}

fn shared_clipboard() -> &'static SharedClipboard {
    static CLIPBOARD: OnceLock<SharedClipboard> = OnceLock::new();
    CLIPBOARD.get_or_init(|| Arc::new(Mutex::new(ServerClipboard::default())))
}

async fn attached_contains(attached_ids: &AttachedSessionIds, session_id: SessionId) -> bool {
    attached_ids.lock().await.contains(&session_id)
}

async fn attached_insert(attached_ids: &AttachedSessionIds, session_id: SessionId) {
    attached_ids.lock().await.insert(session_id);
}

async fn attached_extend(
    attached_ids: &AttachedSessionIds,
    ids: impl IntoIterator<Item = SessionId>,
) {
    attached_ids.lock().await.extend(ids);
}

async fn attached_remove(attached_ids: &AttachedSessionIds, session_id: SessionId) {
    attached_ids.lock().await.remove(&session_id);
}

async fn attached_snapshot(attached_ids: &AttachedSessionIds) -> HashSet<SessionId> {
    attached_ids.lock().await.clone()
}

async fn clear_session_attachment(attachment: &SessionAttachment) {
    *attachment.lock().await = None;
}

async fn remove_from_session_attachment(attachment: &SessionAttachment, session_id: SessionId) {
    let attached_ids = attachment.lock().await.clone();
    if let Some(attached_ids) = attached_ids {
        attached_remove(&attached_ids, session_id).await;
    }
}

/// Start the IPC accept loop on an already-bound listener.
pub async fn start_ipc_server(
    listener: UnixListener,
    server: IpcServerState,
) -> Result<(), ScribeError> {
    let connection_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                if !verify_peer_uid(&stream) {
                    continue;
                }

                let Ok(permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                    warn!("connection limit ({MAX_CONNECTIONS}) reached, rejecting client");
                    continue;
                };

                info!("client connected");
                let server = server.clone();
                tokio::spawn(async move {
                    handle_client(stream, server).await;
                    drop(permit);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// Acquire the server socket with singleton enforcement.
///
/// In normal mode, uses an advisory flock on `server.lock` to serialise
/// the bind-or-connect sequence.  If another server already holds the
/// socket, returns `IpcError` ("already running").  In upgrade mode the
/// lock and liveness check are skipped — the handoff protocol coordinates
/// the two servers, and the old server still holds the lock.
///
/// Returns the lock file guard (must be kept alive) and the bound listener.
pub fn acquire_server_socket(
    socket_path: &Path,
    upgrade_mode: bool,
) -> Result<(Option<nix::fcntl::Flock<std::fs::File>>, UnixListener), ScribeError> {
    // Ensure the parent directory exists with 0700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    if upgrade_mode {
        // Upgrade mode: unconditionally replace the socket.  The handoff
        // protocol has already coordinated with the old server.
        drop(std::fs::remove_file(socket_path));
        return Ok((None, try_bind(socket_path)?));
    }

    // Normal mode: acquire flock then bind-or-connect.
    let lock_path = scribe_common::socket::server_lock_path();
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| ScribeError::Io { source: e })?;

    let lock_file = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, _)| ScribeError::IpcError {
            reason: "another scribe-server is already running (lock held)".into(),
        })?;

    // Try to bind the socket.  If it fails with EADDRINUSE the path
    // already exists; any other error is a real failure.
    match UnixListener::bind(socket_path) {
        Ok(listener) => {
            set_socket_permissions(socket_path);
            Ok((Some(lock_file), listener))
        }
        Err(bind_err) if bind_err.kind() == std::io::ErrorKind::AddrInUse => {
            // Socket file exists — check if another server is alive.
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Err(ScribeError::IpcError {
                    reason: "another scribe-server is already running".into(),
                });
            }
            // Stale socket from a crashed server — remove and retry.
            info!("removing stale server socket");
            drop(std::fs::remove_file(socket_path));
            Ok((Some(lock_file), try_bind(socket_path)?))
        }
        Err(bind_err) => Err(ScribeError::Io { source: bind_err }),
    }
}

/// Bind the Unix socket and set file permissions to 0o600 (defense-in-depth).
fn try_bind(socket_path: &Path) -> Result<UnixListener, ScribeError> {
    let listener = UnixListener::bind(socket_path).map_err(|e| ScribeError::Io { source: e })?;
    set_socket_permissions(socket_path);
    Ok(listener)
}

/// Set socket file permissions to owner-only (defense-in-depth alongside
/// `SO_PEERCRED` UID verification and the 0o700 parent directory).
fn set_socket_permissions(socket_path: &Path) {
    if let Err(e) = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)) {
        warn!(?socket_path, "failed to set socket permissions: {e}");
    }
}

/// Verify the connecting peer has the same UID as this server process.
fn verify_peer_uid(stream: &tokio::net::UnixStream) -> bool {
    let cred: UCred = match stream.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to get peer credentials, rejecting: {e}");
            return false;
        }
    };

    let expected = current_uid();
    if cred.uid() != expected {
        warn!(peer_uid = cred.uid(), expected, "rejected connection from different UID");
        return false;
    }
    true
}

/// Per-client connection handler. Performs `Hello`/`Welcome` handshake, then
/// reads `ClientMessage`s and dispatches them.
async fn handle_client(stream: tokio::net::UnixStream, server: IpcServerState) {
    let (reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let mut reader = reader;

    // Track which sessions this client has attached to, for detach on disconnect.
    let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

    let Some(window_id) =
        establish_client_window(&mut reader, &server, &writer, &attached_ids).await
    else {
        return;
    };

    run_client_message_loop(&mut reader, window_id, &server, &writer, &attached_ids).await;

    detach_client_window(
        window_id,
        &server.live_sessions,
        &server.connected_clients,
        &attached_ids,
    )
    .await;
}

async fn establish_client_window<R>(
    reader: &mut R,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> Option<WindowId>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match read_message::<ClientMessage, _>(reader).await {
        Ok(ClientMessage::Hello { window_id }) => {
            Some(handle_client_hello(window_id, server, writer).await)
        }
        Ok(ClientMessage::CheckForUpdates) => {
            // Transient action: the caller (e.g. the standalone settings
            // window) does not want a registered window. Run the check, send
            // back a single result, and let the connection close without ever
            // entering `connected_clients`.
            handle_transient_check_for_updates(server, writer).await;
            None
        }
        Ok(msg) => Some(handle_legacy_client(msg, server, writer, attached_ids).await),
        Err(ScribeError::Io { .. }) => {
            debug!("client disconnected before Hello");
            None
        }
        Err(e) => {
            warn!("failed to read Hello message: {e}");
            None
        }
    }
}

async fn handle_transient_check_for_updates(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested manual update check");
    let state = server.updater_handle.request_check().await;
    send_message(writer, &ServerMessage::UpdateCheckResult { state }).await;
}

async fn handle_client_hello(
    requested_window_id: Option<WindowId>,
    server: &IpcServerState,
    writer: &SharedWriter,
) -> WindowId {
    let wm = server.workspace_manager.read().await;
    let all_windows = wm.window_ids_with_sessions();

    // Read connected clients first so we can reuse an unconnected window on
    // fresh-launch restarts (no --window-id).
    let connected = server.connected_clients.read().await;
    let (assigned, other_windows) =
        resolve_window_assignment(requested_window_id, &all_windows, &connected);
    drop(connected);
    drop(wm);

    register_connected_client(assigned, &server.connected_clients, writer).await;

    if !other_windows.is_empty() {
        info!(%assigned, other_count = other_windows.len(), "Welcome includes other_windows — client will spawn additional processes");
    }
    let welcome = ServerMessage::Welcome { window_id: assigned, other_windows };
    send_message(writer, &welcome).await;

    info!(%assigned, "client identified via Hello");
    assigned
}

async fn handle_legacy_client(
    msg: ClientMessage,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> WindowId {
    let window_id = WindowId::new();
    register_connected_client(window_id, &server.connected_clients, writer).await;
    info!(%window_id, "legacy client (no Hello), assigned window");

    let mut context = ClientDispatchContext { server, writer, attached_ids, window_id };
    dispatch_message(msg, &mut context).await;
    window_id
}

async fn register_connected_client(
    window_id: WindowId,
    connected_clients: &ConnectedClients,
    writer: &SharedWriter,
) {
    connected_clients.write().await.insert(window_id, Arc::clone(writer));
}

async fn run_client_message_loop<R>(
    reader: &mut R,
    window_id: WindowId,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let msg: ClientMessage = match read_message(reader).await {
            Ok(msg) => msg,
            Err(ScribeError::Io { .. }) => {
                debug!(%window_id, "client disconnected");
                break;
            }
            Err(e) => {
                warn!(%window_id, "failed to read client message: {e}");
                break;
            }
        };

        let mut context = ClientDispatchContext { server, writer, attached_ids, window_id };
        dispatch_message(msg, &mut context).await;
    }
}

async fn detach_client_window(
    window_id: WindowId,
    live_sessions: &LiveSessionRegistry,
    connected_clients: &ConnectedClients,
    attached_ids: &AttachedSessionIds,
) {
    let attached_ids = attached_snapshot(attached_ids).await;
    // Detach all sessions — clear the writer so the reader task stops
    // forwarding output, but keep the session alive for reconnection.
    detach_sessions(live_sessions, &attached_ids).await;
    let last_client_disconnected = {
        let mut connected = connected_clients.write().await;
        connected.remove(&window_id);
        connected.is_empty()
    };
    info!(%window_id, "client removed from connected clients");
    if last_client_disconnected {
        schedule_settings_shutdown_if_no_clients(Arc::clone(connected_clients));
    }
}

/// Clear the client writer for each session so output stops being forwarded.
/// Sessions remain alive in the registry for future client attachment.
async fn detach_sessions(live_sessions: &LiveSessionRegistry, ids: &HashSet<SessionId>) {
    let sessions = live_sessions.read().await;
    for id in ids {
        if let Some(session) = sessions.get(id) {
            *session.client_writer.lock().await = None;
            clear_session_attachment(&session.attachment).await;
            info!(%id, "session detached (client disconnected)");
        }
    }
}

/// Close the singleton settings window once the client registry stays empty
/// long enough to rule out a hot-reload or reconnect race.
fn schedule_settings_shutdown_if_no_clients(connected_clients: ConnectedClients) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if connected_clients.read().await.is_empty() {
            quit_settings_process();
        } else {
            debug!("settings shutdown skipped because a client reconnected");
        }
    });
}

/// Ask the standalone settings process to quit, if it is running.
fn quit_settings_process() {
    let socket_path = scribe_common::socket::settings_socket_path();
    match std::os::unix::net::UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            use std::io::Write as _;

            if let Err(e) = stream.write_all(b"{\"cmd\":\"quit\"}\n") {
                warn!("failed to send quit command to settings: {e}");
            } else {
                debug!("sent quit to settings process after last client disconnect");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            debug!("settings quit skipped because socket connect failed: {e}");
        }
    }
}

/// Extract per-workspace session order from a reported tree and apply it.
fn apply_tab_order_from_tree(wm: &mut WorkspaceManager, tree: &WorkspaceTreeNode) {
    match tree {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, .. } => {
            if !session_ids.is_empty() {
                wm.reorder_sessions(*workspace_id, session_ids);
            }
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            apply_tab_order_from_tree(wm, first);
            apply_tab_order_from_tree(wm, second);
        }
    }
}

/// Dispatch a single `ClientMessage` to the appropriate handler.
async fn dispatch_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        msg @ (ClientMessage::CreateSession { .. }
        | ClientMessage::KeyInput { .. }
        | ClientMessage::CloseSession { .. }
        | ClientMessage::Resize { .. }
        | ClientMessage::AttachSessions { .. }
        | ClientMessage::ConfigReloaded
        | ClientMessage::FocusChanged { .. }
        | ClientMessage::SearchRequest { .. }) => {
            dispatch_session_message(msg, context).await;
        }
        msg @ (ClientMessage::Subscribe { .. }
        | ClientMessage::RequestSnapshot { .. }
        | ClientMessage::CreateWorkspace
        | ClientMessage::ListSessions
        | ClientMessage::ReportWorkspaceTree { .. }) => {
            dispatch_workspace_message(msg, context).await;
        }
        msg @ (ClientMessage::CloseWindow { .. }
        | ClientMessage::QuitAll
        | ClientMessage::TriggerUpdate
        | ClientMessage::DismissUpdate
        | ClientMessage::CheckForUpdates
        | ClientMessage::ListWindows
        | ClientMessage::DispatchAction { .. }) => {
            dispatch_window_message(msg, context).await;
        }
        ClientMessage::Hello { .. } => debug!("unexpected Hello after handshake, ignoring"),
        other => debug!(?other, "unhandled client message"),
    }
}

async fn dispatch_session_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::CreateSession { workspace_id, split_direction, cwd, size, command } => {
            handle_create_session(
                CreateSessionRequest { workspace_id, split_direction, cwd, size, command },
                context,
            )
            .await;
        }
        ClientMessage::KeyInput { session_id, data, dismisses_attention } => {
            handle_key_input(
                session_id,
                &data,
                dismisses_attention,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
        }
        ClientMessage::CloseSession { session_id } => {
            handle_close_session(
                session_id,
                &context.server.workspace_manager,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
            context.server.workspace_manager.write().await.remove_session_from_window(session_id);
        }
        ClientMessage::Resize { session_id, size } => {
            handle_resize(session_id, size, &context.server.live_sessions, context.attached_ids)
                .await;
        }
        ClientMessage::AttachSessions { session_ids, dimensions } => {
            handle_attach_sessions(&session_ids, &dimensions, context).await;
        }
        ClientMessage::ConfigReloaded => {
            handle_config_reloaded(&context.server.session_manager, &context.server.live_sessions)
                .await;
        }
        ClientMessage::FocusChanged { gained, lost } => {
            handle_focus_changed(gained, lost, &context.server.live_sessions, context.attached_ids)
                .await;
        }
        ClientMessage::SearchRequest { session_id, query, limit } => {
            handle_search_request(session_id, query, limit, context).await;
        }
        other => debug!(?other, "ignored non-session client message in session dispatcher"),
    }
}

async fn dispatch_workspace_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::Subscribe { session_ids } => {
            let cap = session_ids.len().min(MAX_SUBSCRIBE_IDS);
            let ids = session_ids.get(..cap).unwrap_or(&session_ids);
            handle_subscribe(
                ids,
                &context.server.workspace_manager,
                context.writer,
                &context.server.live_sessions,
            )
            .await;
        }
        ClientMessage::RequestSnapshot { session_id } => {
            handle_request_snapshot(session_id, context.writer, &context.server.live_sessions)
                .await;
        }
        ClientMessage::CreateWorkspace => {
            handle_create_workspace(&context.server.workspace_manager, context.writer).await;
        }
        ClientMessage::ListSessions => {
            handle_list_sessions(
                &context.server.live_sessions,
                &context.server.workspace_manager,
                context.writer,
                context.window_id,
            )
            .await;
        }
        ClientMessage::ReportWorkspaceTree { tree } => {
            debug!(window_id = %context.window_id, "received workspace tree from client");
            let mut wm = context.server.workspace_manager.write().await;
            apply_tab_order_from_tree(&mut wm, &tree);
            wm.set_workspace_tree(tree.clone());
            wm.set_window_tree(context.window_id, tree);
        }
        other => debug!(?other, "ignored non-workspace client message in workspace dispatcher"),
    }
}

async fn dispatch_window_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::CloseWindow { window_id: target_window } => {
            handle_close_window(
                target_window,
                &context.server.workspace_manager,
                &context.server.live_sessions,
                context.attached_ids,
                context.writer,
            )
            .await;
        }
        ClientMessage::QuitAll => {
            handle_quit_all(context.window_id, &context.server.connected_clients).await;
        }
        ClientMessage::TriggerUpdate => {
            info!(window_id = %context.window_id, "client triggered update");
            context.server.updater_handle.trigger();
        }
        ClientMessage::DismissUpdate => {
            info!(window_id = %context.window_id, "client dismissed update notification");
            context.server.updater_handle.dismiss();
        }
        ClientMessage::CheckForUpdates => {
            info!(window_id = %context.window_id, "client requested manual update check");
            let state = context.server.updater_handle.request_check().await;
            send_message(context.writer, &ServerMessage::UpdateCheckResult { state }).await;
        }
        ClientMessage::ListWindows => {
            handle_list_windows(
                &context.server.connected_clients,
                &context.server.workspace_manager,
                context.writer,
            )
            .await;
        }
        ClientMessage::DispatchAction { window_id: target_window_id, action } => {
            handle_dispatch_action(
                target_window_id,
                action,
                &context.server.connected_clients,
                context.writer,
            )
            .await;
        }
        other => debug!(?other, "ignored non-window client message in window dispatcher"),
    }
}

/// Create a new PTY session, register it, start the reader task.
async fn handle_create_session(
    request: CreateSessionRequest,
    context: &mut ClientDispatchContext<'_>,
) {
    let session_id = match context
        .server
        .session_manager
        .create_session(SessionLaunchRequest {
            workspace_id: request.workspace_id,
            cwd: request.cwd,
            size: request.size,
            command: request.command,
        })
        .await
    {
        Ok(id) => id,
        Err(e) => {
            send_error(context.writer, &format!("failed to create session: {e}")).await;
            return;
        }
    };

    // Register session with workspace manager.  When `split_direction` is
    // `Some` the workspace is auto-created (client just split the window).
    {
        let mut wm = context.server.workspace_manager.write().await;
        wm.add_session(request.workspace_id, session_id, request.split_direction);
        wm.assign_session_to_window(context.window_id, session_id);
    }

    let Some(session) = context.server.session_manager.take_session(session_id).await else {
        send_error(context.writer, "session vanished after creation").await;
        return;
    };

    // Notify client of session creation.
    let creation_msg = ServerMessage::SessionCreated {
        session_id,
        workspace_id: request.workspace_id,
        shell_name: session.shell_name.clone(),
    };
    send_message(context.writer, &creation_msg).await;

    // Send workspace info so the client knows the accent color and name.
    {
        let wm = context.server.workspace_manager.read().await;
        if let Some((name, accent_color, ws_split_dir, project_root)) =
            wm.workspace_info(request.workspace_id)
        {
            let info_msg = ServerMessage::WorkspaceInfo {
                workspace_id: request.workspace_id,
                name,
                accent_color,
                split_direction: ws_split_dir,
                project_root,
            };
            send_message(context.writer, &info_msg).await;
        }
    }

    start_session(
        session_id,
        request.workspace_id,
        session,
        InitialAttachment {
            writer: Some(context.writer),
            attached_ids: Some(context.attached_ids),
        },
        SessionRuntimeContext {
            workspace_manager: &context.server.workspace_manager,
            live_sessions: &context.server.live_sessions,
        },
    )
    .await;
    attached_insert(context.attached_ids, session_id).await;
}

/// Split a `ManagedSession`, register in the live registry, and start
/// the PTY reader task. When `writer` is `None` the session starts in
/// detached mode (PTY reader runs but output is silently discarded until
/// a client attaches).
///
/// The registry insert is performed synchronously (before the PTY reader
/// task is spawned) to eliminate the race where `CloseSession` could arrive
/// before the session is visible in the registry.
async fn start_session(
    session_id: SessionId,
    workspace_id: WorkspaceId,
    session: ManagedSession,
    initial_attachment: InitialAttachment<'_>,
    runtime: SessionRuntimeContext<'_>,
) {
    // Extract all fields from session before partial moves.
    let term = session.term;
    let resize_fd = Arc::new(session.resize_fd);
    let child_pid = session.child_pid;
    let shell_name = session.shell_name;
    let pty = session.pty;
    let handoff_snapshot = session.handoff_snapshot;
    let ansi_processor = session.ansi_processor;
    let osc_parser = session.osc_parser;
    let event_rx = session.event_rx;
    let title = session.title.unwrap_or_else(|| String::from("shell"));
    let codex_task_label = session.codex_task_label;
    let cwd = session.cwd;
    let context = session.context;
    let ai_state = session.ai_state;
    let ai_provider_hint = session.ai_provider_hint;
    let cell_width = session.cell_width;
    let cell_height = session.cell_height;

    let (pty_read, pty_write) = tokio::io::split(session.pty_fd);
    let pty_write = Arc::new(Mutex::new(pty_write));

    // Wrap the client writer in an optional so the reader task can
    // continue running when the client disconnects.
    let client_writer: ClientWriter =
        Arc::new(Mutex::new(initial_attachment.writer.map(Arc::clone)));
    let attachment: SessionAttachment =
        Arc::new(Mutex::new(initial_attachment.attached_ids.map(Arc::clone)));

    let hide_codex_hook_logs = Arc::new(AtomicBool::new(load_hide_codex_hook_logs_setting()));
    let preserve_ai_scrollback = Arc::new(AtomicBool::new(load_preserve_ai_scrollback_setting()));
    let scrollback_lines = Arc::new(AtomicUsize::new(load_scrollback_lines_setting()));
    let ai_provider = ai_state.as_ref().map(|state| state.provider).or(ai_provider_hint);

    let live = LiveSession {
        pty_write: Arc::clone(&pty_write),
        resize_fd,
        term: Arc::clone(&term),
        child_pid,
        client_writer: Arc::clone(&client_writer),
        attachment: Arc::clone(&attachment),
        workspace_id,
        shell_name,
        title,
        codex_task_label,
        cwd,
        context,
        ai_state,
        ai_provider_hint,
        cell_width,
        cell_height,
        pty,
        handoff_snapshot,
        hide_codex_hook_logs: Arc::clone(&hide_codex_hook_logs),
        preserve_ai_scrollback: Arc::clone(&preserve_ai_scrollback),
        scrollback_lines: Arc::clone(&scrollback_lines),
    };

    // Insert into the registry before spawning the PTY reader task so that
    // any concurrent `CloseSession` message sees the session immediately.
    runtime.live_sessions.write().await.insert(session_id, live);

    let state = PtyReaderState {
        session_id,
        child_pid,
        pty_read,
        pty_write,
        term,
        ansi_processor,
        osc_parser,
        event_rx,
        client_writer,
        attachment,
        workspace_manager: Arc::clone(runtime.workspace_manager),
        live_sessions: Arc::clone(runtime.live_sessions),
        clipboard: Arc::clone(shared_clipboard()),
        osc_events: Vec::new(),
        last_proc_cwd: None,
        ed3_filter: Ed3Filter::new(),
        codex_hook_log_filter: CodexHookLogFilter::new(),
        ai_provider,
        cell_width,
        cell_height,
        hide_codex_hook_logs,
        preserve_ai_scrollback,
        scrollback_lines,
        preserved_ai_scrollback: PreservedAiScrollback::default(),
    };

    tokio::spawn(pty_reader_task(state));
}

#[cfg(test)]
fn ai_state_uses_ed3_filter(ai_state: Option<&AiProcessState>) -> bool {
    ai_state.is_some_and(|state| ai_provider_uses_ed3_filter(Some(state.provider)))
}

fn ai_provider_uses_ed3_filter(ai_provider: Option<AiProvider>) -> bool {
    matches!(ai_provider, Some(AiProvider::ClaudeCode | AiProvider::CodexCode))
}

/// Write key input data to the PTY.
async fn handle_key_input(
    session_id: SessionId,
    data: &[u8],
    dismisses_attention: bool,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent KeyInput for unattached session");
        return;
    }

    if data.len() > MAX_KEY_INPUT_BYTES {
        warn!(
            %session_id,
            len = data.len(),
            max = MAX_KEY_INPUT_BYTES,
            "KeyInput payload too large, dropping"
        );
        return;
    }

    let pty_write = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            warn!(%session_id, "KeyInput for unknown session");
            return;
        };

        if dismisses_attention {
            dismiss_persisted_attention_state(session);
        }

        Arc::clone(&session.pty_write)
    };

    let mut pty_write = pty_write.lock().await;
    if let Err(e) = pty_write.write_all(data).await {
        warn!(%session_id, "failed to write to PTY: {e}");
    }
}

fn dismiss_persisted_attention_state(session: &mut LiveSession) {
    let Some(ai_state) = session.ai_state.as_ref() else { return };
    let provider = ai_state.provider;
    if matches!(
        ai_state.state,
        AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
    ) {
        session.ai_provider_hint = Some(provider);
        session.ai_state = None;
    }
}

/// Return `true` when the session has `TermMode::FOCUS_IN_OUT` active.
async fn session_has_focus_mode(session: &LiveSession) -> bool {
    let term = session.term.lock().await;
    term.mode().contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT)
}

/// Write a CSI focus byte sequence to a session's PTY if it has opted in.
async fn send_focus_event(session: &LiveSession, bytes: &[u8]) {
    if session_has_focus_mode(session).await {
        let mut pty_write = session.pty_write.lock().await;
        if let Err(e) = pty_write.write_all(bytes).await {
            debug!("focus event write failed: {e}");
        }
    }
}

/// Send CSI focus events to PTY sessions that have DECSET 1004 enabled.
///
/// When a session has `TermMode::FOCUS_IN_OUT` active, write `\x1b[I`
/// (focus gained) or `\x1b[O` (focus lost) to the PTY so the
/// application can respond (e.g. hide cursor, reduce animation).
async fn handle_focus_changed(
    gained: Option<SessionId>,
    lost: Option<SessionId>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    let sessions = live_sessions.read().await;
    if let Some(lost_id) = lost {
        if attached_contains(attached_ids, lost_id).await {
            if let Some(session) = sessions.get(&lost_id) {
                send_focus_event(session, b"\x1b[O").await;
            }
        }
    }
    if let Some(gained_id) = gained {
        if attached_contains(attached_ids, gained_id).await {
            if let Some(session) = sessions.get(&gained_id) {
                send_focus_event(session, b"\x1b[I").await;
            }
        }
    }
}

/// Send `SIGHUP` to the child process of a handoff-restored session.
///
/// After a hot-reload handoff the `pty` field is `None` because we only
/// received the master fd via `SCM_RIGHTS`, not the original `Pty` object.
/// Without the `Pty`, dropping the `LiveSession` does not send `SIGHUP`
/// to the child. This helper fills that gap so `CloseSession` and
/// `CloseWindow` can clean up handoff-restored sessions correctly.
fn signal_if_handoff_session(session_id: SessionId, session: &LiveSession) {
    if session.pty.is_some() {
        return; // `Pty::Drop` will send SIGHUP.
    }
    let pid = session.child_pid.cast_signed();
    info!(%session_id, pid, "sending SIGHUP to handoff-restored session");
    if let Err(err) = kill(Pid::from_raw(pid), Signal::SIGHUP) {
        warn!(%session_id, pid, %err, "failed to send SIGHUP to child");
    }
}

/// Close a session and clean up. For fresh sessions the `Pty::Drop` inside
/// `LiveSession` sends SIGHUP to the child process; for handoff-restored
/// sessions (`pty: None`) we send SIGHUP explicitly so the child is not
/// leaked. The PTY reader task exits naturally on EOF once the child dies.
async fn handle_close_session(
    session_id: SessionId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent CloseSession for unattached session");
        return;
    }

    let removed = live_sessions.write().await.remove(&session_id);
    if let Some(session) = &removed {
        signal_if_handoff_session(session_id, session);
    }
    // `removed` is dropped here — if `pty` is `Some`, `Pty::Drop` sends SIGHUP.
    drop(removed);
    workspace_manager.write().await.remove_session(session_id);
    attached_remove(attached_ids, session_id).await;
    info!(%session_id, "session closed by client");
}

/// Close a window: destroy every session it owns and remove the window from
/// the workspace manager so it won't be resurrected on the next client launch.
async fn handle_close_window(
    window_id: WindowId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
    writer: &SharedWriter,
) {
    let session_ids = workspace_manager.read().await.sessions_for_window(window_id);
    info!(%window_id, count = session_ids.len(), "closing window — destroying sessions");

    // Destroy each session. For fresh sessions `Pty::Drop` sends SIGHUP;
    // for handoff-restored sessions (`pty: None`) we signal explicitly.
    {
        let mut sessions = live_sessions.write().await;
        for &sid in &session_ids {
            if let Some(session) = sessions.remove(&sid) {
                signal_if_handoff_session(sid, &session);
                // `session` dropped here — `Pty::Drop` fires if `pty` is `Some`.
            }
            attached_remove(attached_ids, sid).await;
        }
    }

    // Remove window and all session→window mappings.
    let mut wm = workspace_manager.write().await;
    for &sid in &session_ids {
        wm.remove_session(sid);
    }
    wm.remove_window(window_id);
    drop(wm);

    send_message(writer, &ServerMessage::WindowClosed { window_id }).await;
}

/// Resize the terminal and PTY.
async fn handle_resize(
    session_id: SessionId,
    size: TerminalSize,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent Resize for unattached session");
        return;
    }

    if !size.has_grid() {
        warn!(%session_id, ?size, "ignoring resize with zero dimension");
        return;
    }

    let (term, resize_fd) = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            warn!(%session_id, "Resize for unknown session");
            return;
        };
        session.cell_width = size.cell_width.max(1);
        session.cell_height = size.cell_height.max(1);
        (Arc::clone(&session.term), Arc::clone(&session.resize_fd))
    };

    // Resize the Term state (lock + drop before any await).
    resize_term(&term, size.cols, size.rows).await;

    // Signal the PTY with TIOCSWINSZ.
    if let Err(e) = set_pty_winsize(resize_fd.as_ref(), size) {
        warn!(%session_id, "TIOCSWINSZ failed: {e}");
    }
}

async fn handle_search_request(
    session_id: SessionId,
    query: String,
    limit: u32,
    context: &ClientDispatchContext<'_>,
) {
    if !attached_contains(context.attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent SearchRequest for unattached session");
        return;
    }

    let sessions = context.server.live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        warn!(%session_id, "SearchRequest for unknown session");
        return;
    };
    let term = Arc::clone(&session.term);
    drop(sessions);

    let matches = {
        let term_guard = term.lock().await;
        let snapshot = snapshot_term(&term_guard);
        search_snapshot(&snapshot, &query, limit)
    };

    let msg = ServerMessage::SearchResults { session_id, query, matches };
    send_message(context.writer, &msg).await;
}

fn search_snapshot(snapshot: &ScreenSnapshot, query: &str, limit: u32) -> Vec<SearchMatch> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }

    let max_matches = limit as usize;
    let cols = usize::from(snapshot.cols);
    let history_rows = snapshot.scrollback_rows as usize;
    let history_rows_i32 = i32::try_from(history_rows).unwrap_or(i32::MAX);
    let mut matches = Vec::new();

    for row in 0..history_rows {
        if matches.len() >= max_matches {
            break;
        }

        let start = row.saturating_mul(cols);
        let end = start.saturating_add(cols);
        let row_i32 = i32::try_from(row).unwrap_or(i32::MAX);
        let absolute_row = -history_rows_i32 + row_i32;
        push_row_matches(
            snapshot.scrollback.get(start..end).unwrap_or(&[]),
            absolute_row,
            &needle,
            &mut matches,
            max_matches,
        );
    }

    for row in 0..usize::from(snapshot.rows) {
        if matches.len() >= max_matches {
            break;
        }

        let start = row.saturating_mul(cols);
        let end = start.saturating_add(cols);
        let row_i32 = i32::try_from(row).unwrap_or(i32::MAX);
        push_row_matches(
            snapshot.cells.get(start..end).unwrap_or(&[]),
            row_i32,
            &needle,
            &mut matches,
            max_matches,
        );
    }

    matches
}

fn push_row_matches(
    row_cells: &[ScreenCell],
    row: i32,
    needle: &[char],
    matches: &mut Vec<SearchMatch>,
    max_matches: usize,
) {
    if row_cells.is_empty() || needle.is_empty() || row_cells.len() < needle.len() {
        return;
    }

    let haystack: Vec<char> =
        row_cells.iter().map(|cell| if cell.c == '\0' { ' ' } else { cell.c }).collect();
    let last_start = haystack.len().saturating_sub(needle.len());

    for start in 0..=last_start {
        if haystack.get(start..start + needle.len()).is_some_and(|window| window == needle) {
            let Some(col_start) = u16::try_from(start).ok() else { break };
            let Some(col_end) = u16::try_from(start + needle.len() - 1).ok() else { break };
            matches.push(SearchMatch { row, col_start, col_end });
            if matches.len() >= max_matches {
                return;
            }
        }
    }
}

/// Terminal dimensions for `Term::resize()`.
struct ResizeDimensions {
    cols: usize,
    lines: usize,
}

impl alacritty_terminal::grid::Dimensions for ResizeDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Lock the `Term` and apply the new dimensions.
pub async fn resize_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    cols: u16,
    rows: u16,
) {
    let mut term_guard = term.lock().await;
    let size = ResizeDimensions { cols: usize::from(cols), lines: usize::from(rows) };
    term_guard.resize(size);
    // Guard dropped here — before any subsequent .await.
}

/// Set PTY window size via `TIOCSWINSZ` ioctl.
///
/// Writes a terminal `Winsize` to the PTY fd, which causes the kernel to send
/// `SIGWINCH` to the foreground process group.
pub fn set_pty_winsize(fd: impl AsFd, size: TerminalSize) -> Result<(), ScribeError> {
    let ws = rustix::termios::Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols.saturating_mul(size.cell_width.max(1)),
        ws_ypixel: size.rows.saturating_mul(size.cell_height.max(1)),
    };

    rustix::termios::tcsetwinsize(fd, ws).map_err(std::io::Error::from).map_err(ScribeError::from)
}

/// Handle `Subscribe` — trigger CWD fallback check for visible sessions.
async fn handle_subscribe(
    session_ids: &[SessionId],
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
) {
    let sessions = live_sessions.read().await;
    for &session_id in session_ids {
        let Some(session) = sessions.get(&session_id) else {
            continue;
        };

        let msg = {
            let mut wm = workspace_manager.write().await;
            wm.check_cwd_fallback(session_id, session.child_pid)
        };

        if let Some(named_msg) = msg {
            send_message(writer, &named_msg).await;
        }
    }
}

/// Handle `RequestSnapshot` — snapshot the terminal and send it to the client.
async fn handle_request_snapshot(
    session_id: SessionId,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
) {
    let sessions = live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        send_error(writer, &format!("RequestSnapshot for unknown session {session_id}")).await;
        return;
    };

    let term = session.term.lock().await;
    let snapshot = snapshot_term(&term);
    drop(term);
    drop(sessions);

    let msg = ServerMessage::ScreenSnapshot { session_id, snapshot };
    send_message(writer, &msg).await;
}

/// Handle `CreateWorkspace` — create a new workspace and send info to the client.
async fn handle_create_workspace(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) {
    let mut wm = workspace_manager.write().await;
    let workspace_id = wm.create_workspace();
    let (name, accent_color, split_direction, project_root) = wm
        .workspace_info(workspace_id)
        .unwrap_or_else(|| (None, String::from("#a78bfa"), None, None));
    drop(wm);

    let msg = ServerMessage::WorkspaceInfo {
        workspace_id,
        name,
        accent_color,
        split_direction,
        project_root,
    };
    send_message(writer, &msg).await;
}

/// Handle `ListSessions` — reply with all live sessions and their workspace info.
async fn handle_list_sessions(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    window_id: WindowId,
) {
    let sessions = live_sessions.read().await;
    let wm = workspace_manager.read().await;

    // Filter sessions to those belonging to this window.
    let window_session_ids = wm.sessions_for_window(window_id);
    let has_window_sessions = !window_session_ids.is_empty();

    let build_info = |sid: SessionId, s: &LiveSession| SessionInfo {
        session_id: sid,
        workspace_id: s.workspace_id,
        shell_name: s.shell_name.clone(),
        title: Some(s.title.clone()),
        context: s.context.clone(),
        codex_task_label: s.codex_task_label.clone(),
        cwd: s.cwd.clone(),
        git_branch: s.cwd.as_deref().and_then(detect_git_branch),
        ai_state: s.ai_state.clone(),
        ai_provider_hint: s.ai_state.as_ref().map(|state| state.provider).or(s.ai_provider_hint),
    };

    let infos: Vec<SessionInfo> = if has_window_sessions {
        // Return only this window's sessions.
        window_session_ids
            .iter()
            .filter_map(|&sid| sessions.get(&sid).map(|s| build_info(sid, s)))
            .collect()
    } else {
        // No window-specific sessions — return all unowned sessions (legacy
        // fallback or first-time connect with existing sessions).
        sessions
            .iter()
            .filter(|&(&sid, _)| wm.window_for_session(sid).is_none())
            .map(|(&id, s)| build_info(id, s))
            .collect()
    };

    // Batch per-workspace metadata into the SessionList so clients do not need
    // a separate per-session WorkspaceInfo fan-out during reattach.
    let mut seen = HashSet::new();
    let mut workspaces: Vec<WorkspaceListEntry> = Vec::new();
    for info in &infos {
        if !seen.insert(info.workspace_id) {
            continue;
        }
        if let Some((name, accent_color, split_direction, project_root)) =
            wm.workspace_info(info.workspace_id)
        {
            workspaces.push(WorkspaceListEntry {
                workspace_id: info.workspace_id,
                name,
                accent_color,
                split_direction,
                project_root,
            });
        }
    }
    let workspace_tree = wm.window_tree(window_id).cloned();
    drop(wm);
    drop(sessions);

    let list_msg = ServerMessage::SessionList { sessions: infos, workspace_tree, workspaces };
    send_message(writer, &list_msg).await;
}

/// Handle `AttachSessions` — take ownership of detached sessions, set the
/// client writer, and send back session + workspace info for each.
async fn handle_attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    context: &mut ClientDispatchContext<'_>,
) {
    let attached = crate::attach_flow::attach_sessions(
        session_ids,
        dimensions,
        &context.server.live_sessions,
        crate::attach_flow::AttachClientContext {
            writer: context.writer,
            attached_ids: context.attached_ids,
        },
    )
    .await;
    attached_extend(context.attached_ids, attached).await;
}

/// Handle `ConfigReloaded` — reload the config file and apply live changes.
async fn handle_config_reloaded(
    session_manager: &Arc<SessionManager>,
    live_sessions: &LiveSessionRegistry,
) {
    let cfg = match crate::config::load_config() {
        Ok(cfg) => {
            info!("config reloaded successfully via client request");
            cfg
        }
        Err(e) => {
            warn!("config reload failed: {e}");
            return;
        }
    };

    let new_scrollback = usize::try_from(cfg.scrollback_lines).unwrap_or(usize::MAX);
    session_manager.set_scrollback_lines(new_scrollback);

    let term_config = build_term_config(new_scrollback);
    let sessions = live_sessions.read().await;
    for session in sessions.values() {
        session.term.lock().await.set_options(term_config.clone());
        session.scrollback_lines.store(new_scrollback, Ordering::Relaxed);
        session.hide_codex_hook_logs.store(cfg.ai_terminal.hide_codex_hook_logs, Ordering::Relaxed);
        session
            .preserve_ai_scrollback
            .store(cfg.ai_terminal.preserve_ai_scrollback, Ordering::Relaxed);
    }
    info!(
        scrollback_lines = new_scrollback,
        hide_codex_hook_logs = cfg.ai_terminal.hide_codex_hook_logs,
        preserve_ai_scrollback = cfg.ai_terminal.preserve_ai_scrollback,
        sessions = sessions.len(),
        "scrollback updated on live sessions"
    );
}

fn load_hide_codex_hook_logs_setting() -> bool {
    match scribe_common::config::load_config() {
        Ok(config) => config.terminal.ai_session.hide_codex_hook_logs,
        Err(e) => {
            warn!("failed to load codex hook log filter setting: {e}");
            false
        }
    }
}

fn load_preserve_ai_scrollback_setting() -> bool {
    match scribe_common::config::load_config() {
        Ok(config) => config.terminal.ai_session.preserve_ai_scrollback,
        Err(e) => {
            warn!("failed to load preserve_ai_scrollback setting: {e}");
            true
        }
    }
}

fn load_scrollback_lines_setting() -> usize {
    match scribe_common::config::load_config() {
        Ok(config) => usize::try_from(config.terminal.scrollback_lines).unwrap_or(usize::MAX),
        Err(e) => {
            warn!("failed to load scrollback_lines setting: {e}");
            10_000
        }
    }
}

/// Broadcast `QuitRequested` to all connected clients, including the sender.
async fn handle_quit_all(sender_window_id: WindowId, connected_clients: &ConnectedClients) {
    info!(%sender_window_id, "QuitAll requested — broadcasting QuitRequested");
    let clients = connected_clients.read().await;
    let quit_msg = ServerMessage::QuitRequested;
    for writer in clients.values() {
        send_message(writer, &quit_msg).await;
    }
}

async fn handle_list_windows(
    connected_clients: &ConnectedClients,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) {
    let connected = connected_clients.read().await;
    let wm = workspace_manager.read().await;

    let mut window_ids: HashSet<WindowId> = wm.window_ids_with_sessions();
    window_ids.extend(connected.keys().copied());

    let mut windows: Vec<WindowInfo> = window_ids
        .into_iter()
        .map(|window_id| WindowInfo {
            window_id,
            session_count: wm.sessions_for_window(window_id).len(),
            connected: connected.contains_key(&window_id),
        })
        .collect();
    windows.sort_by_key(|info| info.window_id.to_full_string());
    drop(wm);
    drop(connected);

    send_message(writer, &ServerMessage::WindowList { windows }).await;
}

async fn handle_dispatch_action(
    requested_window_id: Option<WindowId>,
    action: AutomationAction,
    connected_clients: &ConnectedClients,
    writer: &SharedWriter,
) {
    let connected = connected_clients.read().await;
    let target_window_id = requested_window_id.map_or_else(
        || {
            let mut ids: Vec<WindowId> = connected.keys().copied().collect();
            ids.sort_by_key(|window_id| window_id.to_full_string());
            ids.first().copied()
        },
        |window_id| connected.contains_key(&window_id).then_some(window_id),
    );

    let target_writer = target_window_id.and_then(|window_id| connected.get(&window_id).cloned());
    drop(connected);

    let Some(target_window_id) = target_window_id else {
        if let Some(window_id) = requested_window_id {
            send_error(writer, &format!("window not connected: {window_id}")).await;
        } else {
            send_error(writer, "no connected windows").await;
        }
        return;
    };
    let Some(target_writer) = target_writer else {
        send_error(writer, &format!("window not connected: {target_window_id}")).await;
        return;
    };

    if !try_send_message(&target_writer, &ServerMessage::RunAction { action }).await {
        send_error(writer, &format!("failed to dispatch action to {target_window_id}")).await;
        return;
    }

    send_message(writer, &ServerMessage::ActionDispatched { window_id: target_window_id }).await;
}

/// Send a `ServerMessage` to the client, logging errors.
pub async fn send_message(writer: &SharedWriter, msg: &ServerMessage) {
    let _ = try_send_message(writer, msg).await;
}

async fn try_send_message(writer: &SharedWriter, msg: &ServerMessage) -> bool {
    let mut w = writer.lock().await;
    match write_message(&mut *w, msg).await {
        Ok(()) => true,
        Err(e) => {
            warn!("failed to send message to client: {e}");
            false
        }
    }
}

/// Send a `ServerMessage` via the optional client writer. No-op when the
/// session is detached (writer is `None`).
async fn send_to_client(client_writer: &ClientWriter, msg: &ServerMessage) {
    let guard = client_writer.lock().await;
    if let Some(writer) = guard.as_ref() {
        let mut w = writer.lock().await;
        if let Err(e) = write_message(&mut *w, msg).await {
            warn!("failed to send message to client: {e}");
        }
    }
}

/// Send an error message to the client.
async fn send_error(writer: &SharedWriter, message: &str) {
    let msg = ServerMessage::Error { message: message.to_owned() };
    send_message(writer, &msg).await;
}

// ── PTY reader task ─────────────────────────────────────────────

/// The dual-path read loop: raw bytes to UI (fast path) + Term state + metadata.
///
/// Uses `ClientWriter` (optional) so the task keeps running even when no
/// client is connected. Output is silently dropped when detached, but the
/// `Term` state continues to be updated.
async fn pty_reader_task(mut state: PtyReaderState) {
    let mut buf = vec![0u8; PTY_READ_BUF_SIZE];

    loop {
        match next_pty_read_action(&mut state, &mut buf).await {
            PtyReadAction::Continue => {}
            PtyReadAction::End => break,
            PtyReadAction::Data(bytes_read) => {
                let Some(bytes) = buf.get(..bytes_read) else { break };
                process_pty_chunk(&mut state, bytes).await;
            }
        }
    }

    flush_pending_codex_output(&mut state).await;
    finalize_pty_reader(state).await;
}

enum PtyReadAction {
    Continue,
    Data(usize),
    End,
}

async fn next_pty_read_action(state: &mut PtyReaderState, buf: &mut [u8]) -> PtyReadAction {
    let read_result = if let Some(deadline) = state.ansi_processor.sync_timeout().sync_timeout() {
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => {
                stop_term_sync(&state.term, &mut state.ansi_processor).await;
                return PtyReadAction::Continue;
            }
            result = read_pty_bytes(&mut state.pty_read, buf) => result,
        }
    } else {
        read_pty_bytes(&mut state.pty_read, buf).await
    };

    match read_result {
        ReadResult::Data(n) => PtyReadAction::Data(n),
        ReadResult::Eof => PtyReadAction::End,
        ReadResult::Err(e) => {
            warn!(session_id = %state.session_id, "PTY read error: {e}");
            PtyReadAction::End
        }
    }
}

async fn process_pty_chunk(state: &mut PtyReaderState, bytes: &[u8]) {
    capture_osc_metadata_events(state, bytes);
    let effective = apply_pty_filters(state, bytes);
    let suppressed_ed3 = state.ed3_filter.take_suppressed();
    let trimmed_rows = if suppressed_ed3 { handle_suppressed_ai_ed3(state).await } else { None };

    if let Some(rows) = trimmed_rows {
        send_trim_scrollback(&state.client_writer, state.session_id, rows).await;
    }

    // Step 1: Fast path — forward (possibly filtered) bytes to UI client.
    send_pty_output(&state.client_writer, state.session_id, effective.as_ref()).await;

    // Step 1b: If ED 3 was suppressed, tell the client to snap the
    // viewport to bottom.  A real ED 3 would have reset `display_offset`
    // to 0 inside `clear_history()`, but since we stripped the sequence,
    // the client's Term never ran that code.
    if suppressed_ed3 {
        let msg = ServerMessage::ScrollBottom { session_id: state.session_id };
        send_to_client(&state.client_writer, &msg).await;
    }

    // Step 2: State path — feed (possibly filtered) bytes into Term.
    feed_term(&state.term, &mut state.ansi_processor, effective.as_ref()).await;

    // Steps 3–5: Metadata uses original bytes (OSC parser doesn't care about CSI ED 3).
    process_metadata_events(state).await;
}

fn apply_pty_filters<'a>(state: &mut PtyReaderState, bytes: &'a [u8]) -> Cow<'a, [u8]> {
    let chunk_has_ed3_provider = chunk_mentions_ed3_provider(&state.osc_events);
    let preserve = state.preserve_ai_scrollback.load(Ordering::Relaxed);
    if !preserve {
        state.preserved_ai_scrollback.reset();
    }
    let ed3_output =
        if preserve && should_apply_ed3_filter(state.ai_provider, chunk_has_ed3_provider) {
            state.ed3_filter.filter(bytes)
        } else {
            scribe_pty::ed3_filter::Ed3Output::Unchanged(bytes)
        };
    let after_ed3 = match ed3_output {
        scribe_pty::ed3_filter::Ed3Output::Unchanged(filtered_bytes) => {
            Cow::Borrowed(filtered_bytes)
        }
        scribe_pty::ed3_filter::Ed3Output::Filtered(filtered_bytes) => Cow::Owned(filtered_bytes),
    };
    if !should_apply_codex_hook_log_filter(state) {
        return after_ed3;
    }

    match state.codex_hook_log_filter.filter(after_ed3.as_ref()) {
        scribe_pty::codex_hook_log_filter::CodexHookLogOutput::Unchanged(_) => after_ed3,
        scribe_pty::codex_hook_log_filter::CodexHookLogOutput::Filtered(filtered_bytes) => {
            Cow::Owned(filtered_bytes)
        }
    }
}

async fn flush_pending_codex_output(state: &mut PtyReaderState) {
    let Some(flushed) = state.codex_hook_log_filter.flush() else { return };
    send_pty_output(&state.client_writer, state.session_id, &flushed).await;
    feed_term(&state.term, &mut state.ansi_processor, &flushed).await;
}

async fn handle_suppressed_ai_ed3(state: &mut PtyReaderState) -> Option<usize> {
    let current_history = {
        let term_guard = state.term.lock().await;
        term_guard.grid().history_size()
    };
    let trim_rows = state.preserved_ai_scrollback.trim_target(current_history);
    if let Some(kept_rows) = trim_rows {
        trim_term_scrollback(
            &state.term,
            kept_rows,
            state.scrollback_lines.load(Ordering::Relaxed),
        )
        .await;
    }
    trim_rows
}

async fn trim_term_scrollback(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    kept_rows: usize,
    max_rows: usize,
) {
    let mut term_guard = term.lock().await;
    trim_term_scrollback_inner(&mut term_guard, kept_rows, max_rows);
}

fn trim_term_scrollback_inner(
    term: &mut alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>,
    kept_rows: usize,
    max_rows: usize,
) {
    let kept_rows = kept_rows.min(max_rows);
    let grid = term.grid_mut();
    grid.update_history(kept_rows);
    grid.update_history(max_rows);
}

async fn finalize_pty_reader(state: PtyReaderState) {
    let exit_msg = ServerMessage::SessionExited { session_id: state.session_id, exit_code: None };
    send_to_client(&state.client_writer, &exit_msg).await;
    remove_from_session_attachment(&state.attachment, state.session_id).await;
    state.live_sessions.write().await.remove(&state.session_id);
    let mut workspace_manager = state.workspace_manager.write().await;
    workspace_manager.remove_session(state.session_id);
    workspace_manager.remove_session_from_window(state.session_id);
    info!(session_id = %state.session_id, "PTY reader task exited");
}

/// Result of a PTY read attempt.
enum ReadResult {
    Data(usize),
    Eof,
    Err(std::io::Error),
}

/// Read bytes from the PTY read half.
async fn read_pty_bytes(
    pty_read: &mut ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    buf: &mut [u8],
) -> ReadResult {
    use tokio::io::AsyncReadExt as _;

    match pty_read.read(buf).await {
        Ok(0) => ReadResult::Eof,
        Ok(n) => ReadResult::Data(n),
        Err(e) => ReadResult::Err(e),
    }
}

/// Send raw PTY output to the client (fast path). No-op when detached.
async fn send_pty_output(client_writer: &ClientWriter, session_id: SessionId, bytes: &[u8]) {
    let msg = ServerMessage::PtyOutput { session_id, data: bytes.to_vec() };
    send_to_client(client_writer, &msg).await;
}

async fn send_trim_scrollback(
    client_writer: &ClientWriter,
    session_id: SessionId,
    history_rows: usize,
) {
    let msg = ServerMessage::TrimScrollback {
        session_id,
        history_rows: u32::try_from(history_rows).unwrap_or(u32::MAX),
    };
    send_to_client(client_writer, &msg).await;
}

/// Feed bytes into the terminal emulator via the ANSI processor.
/// The Term mutex lock is held only during `advance()` — dropped before returning.
async fn feed_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
) {
    let mut term_guard = term.lock().await;
    ansi_processor.advance(&mut *term_guard, bytes);
    // Guard dropped here — before any subsequent .await.
}

/// Flush a synchronized update after its timeout elapses.
async fn stop_term_sync(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: &mut AnsiProcessor,
) {
    let mut term_guard = term.lock().await;
    ansi_processor.stop_sync(&mut *term_guard);
}

fn capture_osc_metadata_events(state: &mut PtyReaderState, bytes: &[u8]) {
    run_osc_interceptor(&mut state.osc_parser, bytes, &mut state.osc_events);
}

/// Parse OSC sequences from bytes using the interceptor. Pure computation, no async.
///
/// Events are pushed into `out`, which the caller clears between iterations to
/// avoid allocating a new `Vec` on every PTY read.
fn run_osc_interceptor(osc_parser: &mut VteParser, bytes: &[u8], out: &mut Vec<MetadataEvent>) {
    let mut interceptor = OscInterceptor::new(out);
    osc_parser.advance(&mut interceptor, bytes);
}

/// Run the OSC interceptor, drain the metadata channel, classify events,
/// and — if a title changed but no OSC 7 arrived — fall back to
/// `/proc/pid/cwd` for CWD detection.
async fn process_metadata_events(state: &mut PtyReaderState) {
    let mut saw_title_change = false;
    let mut saw_cwd_change = false;

    // OSC interceptor events captured before the UI fast path.
    let mut events_this_iter = std::mem::take(&mut state.osc_events);
    for event in events_this_iter.drain(..) {
        handle_session_event(
            SessionEvent::Metadata(event),
            state,
            &mut saw_title_change,
            &mut saw_cwd_change,
        )
        .await;
    }
    state.osc_events = events_this_iter;

    // ScribeEventListener channel events.
    while let Ok(event) = state.event_rx.try_recv() {
        handle_session_event(event, state, &mut saw_title_change, &mut saw_cwd_change).await;
    }

    // Fallback: title changed but no OSC 7 → read /proc/pid/cwd.
    if saw_title_change && !saw_cwd_change {
        check_proc_cwd(state).await;
    }
}

async fn handle_session_event(
    event: SessionEvent,
    state: &mut PtyReaderState,
    saw_title_change: &mut bool,
    saw_cwd_change: &mut bool,
) {
    match event {
        SessionEvent::Metadata(event) => {
            update_ai_provider_state(state, &event);
            classify_event(&event, saw_title_change, saw_cwd_change, &mut state.last_proc_cwd);
            send_metadata_event(
                event,
                state.session_id,
                &state.client_writer,
                &state.workspace_manager,
                &state.live_sessions,
            )
            .await;
        }
        SessionEvent::ClipboardStore(kind, text) => {
            state.clipboard.lock().await.store(kind, text);
        }
        SessionEvent::ClipboardLoad(kind, format) => {
            let text = {
                let clipboard = state.clipboard.lock().await;
                clipboard.load(kind).to_owned()
            };
            let response = format(&text);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
        }
        SessionEvent::ColorRequest(index, format) => {
            let color = current_term_color(&state.term, index).await;
            let response = format(color);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
        }
        SessionEvent::PtyWrite(text) => {
            write_term_response(&state.pty_write, state.session_id, text.as_bytes()).await;
        }
        SessionEvent::TextAreaSizeRequest(format) => {
            let size = current_window_size(state).await;
            let response = format(size);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
        }
    }
}

fn update_ai_provider_state(state: &mut PtyReaderState, event: &MetadataEvent) {
    // Don't clear ai_provider on inactive — the ED 3 filter must remain
    // engaged across tool restarts.  Without this, `codex --resume` sends
    // ED 3 before re-identifying via OSC 1337, slipping through the filter
    // and wiping scrollback.  The AiStateCleared event is still forwarded
    // to the client for UI tracking; only the PTY reader's filter decision
    // is affected.
    match event {
        MetadataEvent::AiStateChanged(ai_state) => {
            state.ai_provider = Some(ai_state.provider);
        }
        MetadataEvent::AiStateCleared => {
            state.preserved_ai_scrollback.reset();
        }
        _ => {}
    }
}

fn should_apply_ed3_filter(ai_provider: Option<AiProvider>, chunk_has_ed3_provider: bool) -> bool {
    ai_provider_uses_ed3_filter(ai_provider) || chunk_has_ed3_provider
}

fn should_apply_codex_hook_log_filter(state: &PtyReaderState) -> bool {
    if state.codex_hook_log_filter.has_pending() {
        return true;
    }

    state.hide_codex_hook_logs.load(Ordering::Relaxed)
}

fn chunk_mentions_ed3_provider(events: &[MetadataEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            MetadataEvent::AiStateChanged(ai_state)
                if ai_provider_uses_ed3_filter(Some(ai_state.provider))
        )
    })
}

async fn current_term_color(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    index: usize,
) -> alacritty_terminal::vte::ansi::Rgb {
    let term_guard = term.lock().await;
    if index >= alacritty_terminal::term::color::COUNT {
        return alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 };
    }

    if let Some(color) = term_guard.colors()[index] {
        return color;
    }
    drop(term_guard);

    fallback_term_color(index).unwrap_or(alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 })
}

fn fallback_term_color(index: usize) -> Option<alacritty_terminal::vte::ansi::Rgb> {
    let config = scribe_config::load_config().ok()?;
    let theme = scribe_config::resolve_theme(&config);

    theme_color_for_index(&theme, index).map(theme_color_to_rgb)
}

fn theme_color_for_index(theme: &scribe_common::theme::Theme, index: usize) -> Option<[f32; 4]> {
    use alacritty_terminal::vte::ansi::NamedColor;

    match index {
        0..=15 => theme.ansi_colors.get(index).copied(),
        x if x == NamedColor::Foreground as usize || x == NamedColor::BrightForeground as usize => {
            Some(theme.foreground)
        }
        x if x == NamedColor::Background as usize => Some(theme.background),
        x if x == NamedColor::Cursor as usize => Some(theme.cursor),
        x if x == NamedColor::DimForeground as usize => Some(dim_theme_color(theme.foreground)),
        _ => dim_ansi_theme_color(theme, index),
    }
}

fn dim_ansi_theme_color(theme: &scribe_common::theme::Theme, index: usize) -> Option<[f32; 4]> {
    use alacritty_terminal::vte::ansi::NamedColor;

    let base_index = match index {
        x if x == NamedColor::DimBlack as usize => 0,
        x if x == NamedColor::DimRed as usize => 1,
        x if x == NamedColor::DimGreen as usize => 2,
        x if x == NamedColor::DimYellow as usize => 3,
        x if x == NamedColor::DimBlue as usize => 4,
        x if x == NamedColor::DimMagenta as usize => 5,
        x if x == NamedColor::DimCyan as usize => 6,
        x if x == NamedColor::DimWhite as usize => 7,
        _ => return None,
    };

    theme.ansi_colors.get(base_index).copied().map(dim_theme_color)
}

fn dim_theme_color(color: [f32; 4]) -> [f32; 4] {
    [color[0] * 0.67, color[1] * 0.67, color[2] * 0.67, color[3]]
}

fn theme_color_to_rgb(color: [f32; 4]) -> alacritty_terminal::vte::ansi::Rgb {
    alacritty_terminal::vte::ansi::Rgb {
        r: scribe_common::theme::channel_to_u8(color[0]),
        g: scribe_common::theme::channel_to_u8(color[1]),
        b: scribe_common::theme::channel_to_u8(color[2]),
    }
}

async fn current_window_size(state: &PtyReaderState) -> alacritty_terminal::event::WindowSize {
    let term_guard = state.term.lock().await;
    let rows = term_guard.grid().screen_lines();
    let cols = term_guard.grid().columns();
    alacritty_terminal::event::WindowSize {
        num_lines: u16::try_from(rows).unwrap_or(u16::MAX),
        num_cols: u16::try_from(cols).unwrap_or(u16::MAX),
        cell_width: state.cell_width.max(1),
        cell_height: state.cell_height.max(1),
    }
}

async fn write_term_response(
    pty_write: &Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    session_id: SessionId,
    data: &[u8],
) {
    let mut writer = pty_write.lock().await;
    if let Err(e) = writer.write_all(data).await {
        debug!(%session_id, error = %e, "failed to write terminal response to PTY");
    }
}

/// Update the `saw_title_change` / `saw_cwd_change` flags and keep
/// `last_proc_cwd` in sync with any OSC 7 events.
fn classify_event(
    event: &MetadataEvent,
    saw_title: &mut bool,
    saw_cwd: &mut bool,
    last_cwd: &mut Option<std::path::PathBuf>,
) {
    match event {
        MetadataEvent::TitleChanged(_) => *saw_title = true,
        MetadataEvent::CwdChanged(cwd) => {
            *saw_cwd = true;
            *last_cwd = Some(cwd.clone());
        }
        _ => {}
    }
}

/// Read `/proc/{pid}/cwd` and synthesise a `CwdChanged` event when the CWD
/// has changed since the last check.  Called when the shell emits a title
/// change (OSC 0) but no OSC 7, so workspace naming still works for shells
/// that only set the window title in PS1.
#[cfg(target_os = "linux")]
async fn check_proc_cwd(state: &mut PtyReaderState) {
    let proc_cwd = std::path::PathBuf::from(format!("/proc/{}/cwd", state.child_pid));
    let Ok(cwd) = std::fs::read_link(&proc_cwd) else {
        return;
    };
    if state.last_proc_cwd.as_ref() == Some(&cwd) {
        return;
    }
    state.last_proc_cwd = Some(cwd.clone());
    let event = MetadataEvent::CwdChanged(cwd);
    send_metadata_event(
        event,
        state.session_id,
        &state.client_writer,
        &state.workspace_manager,
        &state.live_sessions,
    )
    .await;
}

/// macOS fallback: use `proc_pidinfo` with `PROC_PIDVNODEPATHINFO` to read
/// the child process CWD, then synthesise a `CwdChanged` event when it differs
/// from the last known value.
#[cfg(target_os = "macos")]
async fn check_proc_cwd(state: &mut PtyReaderState) {
    let Some(cwd) = crate::macos_proc::macos_proc_cwd(state.child_pid) else {
        return;
    };
    if state.last_proc_cwd.as_ref() == Some(&cwd) {
        return;
    }
    state.last_proc_cwd = Some(cwd.clone());
    let event = MetadataEvent::CwdChanged(cwd);
    send_metadata_event(
        event,
        state.session_id,
        &state.client_writer,
        &state.workspace_manager,
        &state.live_sessions,
    )
    .await;
}

/// Stub for platforms other than Linux and macOS — no CWD fallback available.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn check_proc_cwd(_state: &mut PtyReaderState) {}

/// Maximum number of parent directories to traverse when searching for a
/// `.git/HEAD` file. Prevents unbounded walks on deep or unusual directory
/// trees where no git repository is ever found.
const GIT_WALK_DEPTH_LIMIT: usize = 50;

/// Detect the current git branch by walking up from `cwd` looking for `.git/HEAD`.
///
/// Returns `Some(branch_name)` if on a named branch, `Some(short_sha)` if in
/// detached HEAD state, or `None` if not inside a git repository.
/// Stops after `GIT_WALK_DEPTH_LIMIT` iterations to avoid walking all the
/// way to `/` on very deep directory trees.
pub fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    let mut depth = 0usize;
    loop {
        if depth >= GIT_WALK_DEPTH_LIMIT {
            return None;
        }
        depth += 1;

        let head = dir.join(".git/HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            return content
                .strip_prefix("ref: refs/heads/")
                .map(|b| b.trim().to_owned())
                .or_else(|| Some(content.trim().chars().take(8).collect()));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Persist metadata from a `ServerMessage` into the live session registry.
async fn persist_session_metadata(
    server_msg: &ServerMessage,
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) {
    match server_msg {
        ServerMessage::TitleChanged { title, .. } if !title.trim().is_empty() => {
            update_live_session(session_id, live_sessions, |session| {
                title.clone_into(&mut session.title);
            })
            .await;
        }
        ServerMessage::CodexTaskLabelChanged { task_label, .. }
            if !task_label.trim().is_empty() =>
        {
            update_live_session(session_id, live_sessions, |session| {
                session.codex_task_label = Some(task_label.clone());
            })
            .await;
        }
        ServerMessage::CodexTaskLabelCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.codex_task_label = None;
            })
            .await;
        }
        ServerMessage::CwdChanged { cwd, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.cwd = Some(cwd.clone());
            })
            .await;
        }
        ServerMessage::SessionContextChanged { context, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.context = Some(context.clone());
            })
            .await;
        }
        ServerMessage::AiStateChanged { ai_state, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.ai_state = Some(ai_state.clone());
            })
            .await;
        }
        ServerMessage::AiStateCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.ai_state = None;
            })
            .await;
        }
        _ => {}
    }
}

async fn update_live_session(
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
    update: impl FnOnce(&mut LiveSession),
) {
    if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
        update(session);
    }
}

/// Convert a `MetadataEvent` to a `ServerMessage` and send it.
/// For `CwdChanged`, also notifies the workspace manager and sends git branch.
/// Workspace naming always runs (even when detached) so names are ready on
/// reconnect. Client messages are only sent when attached.
async fn send_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
    client_writer: &ClientWriter,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
) {
    let (server_msg, cwd_for_workspace) = convert_metadata_event(event, session_id);

    persist_session_metadata(&server_msg, session_id, live_sessions).await;

    send_to_client(client_writer, &server_msg).await;

    if let Some(cwd) = cwd_for_workspace {
        // Send git branch information for the new CWD.
        let branch = detect_git_branch(&cwd);
        let git_msg = ServerMessage::GitBranch { session_id, branch };
        send_to_client(client_writer, &git_msg).await;

        // Always update workspace naming, even when detached.
        let named_msg = {
            let mut wm = workspace_manager.write().await;
            wm.on_cwd_changed(session_id, &cwd)
        };
        if let Some(msg) = named_msg {
            send_to_client(client_writer, &msg).await;
        }
    }
}

/// Convert a `MetadataEvent` to a `ServerMessage` and an optional CWD.
/// The second tuple element is `Some(cwd)` only for `CwdChanged` events,
/// which also need workspace naming and git-branch updates.
fn convert_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
) -> (ServerMessage, Option<std::path::PathBuf>) {
    match event {
        MetadataEvent::CwdChanged(cwd) => {
            let msg = ServerMessage::CwdChanged { session_id, cwd: cwd.clone() };
            (msg, Some(cwd))
        }
        MetadataEvent::SessionContextChanged(context) => {
            (ServerMessage::SessionContextChanged { session_id, context }, None)
        }
        MetadataEvent::TitleChanged(title) => {
            (ServerMessage::TitleChanged { session_id, title }, None)
        }
        MetadataEvent::CodexTaskLabelChanged(task_label) => {
            (ServerMessage::CodexTaskLabelChanged { session_id, task_label }, None)
        }
        MetadataEvent::CodexTaskLabelCleared => {
            (ServerMessage::CodexTaskLabelCleared { session_id }, None)
        }
        MetadataEvent::AiStateChanged(ai_state) => {
            (ServerMessage::AiStateChanged { session_id, ai_state }, None)
        }
        MetadataEvent::AiStateCleared => (ServerMessage::AiStateCleared { session_id }, None),
        MetadataEvent::Bell => (ServerMessage::Bell { session_id }, None),
        MetadataEvent::PromptMark { kind, click_events, exit_code } => {
            (ServerMessage::PromptMark { session_id, kind, click_events, exit_code }, None)
        }
        MetadataEvent::PromptReceived { provider, text } => {
            (ServerMessage::PromptReceived { session_id, provider, text }, None)
        }
    }
}

// ── Handoff helpers ──────────────────────────────────────────────

/// Create a new, empty live session registry.
pub fn new_live_session_registry() -> LiveSessionRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Create a new empty `ConnectedClients` registry.
pub fn new_connected_clients() -> ConnectedClients {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Serialise all live sessions for a hot-reload handoff.
///
/// Returns `(sessions, fds)` where the fds are in the same order as the
/// session vec. The caller must send these fds via `SCM_RIGHTS`.
pub async fn serialize_live_for_handoff(
    live_sessions: &LiveSessionRegistry,
) -> (Vec<HandoffSession>, Vec<Arc<OwnedFd>>) {
    let sessions = live_sessions.read().await;
    let mut handoff_sessions = Vec::with_capacity(sessions.len());
    let mut fds = Vec::with_capacity(sessions.len());

    for (&session_id, live) in sessions.iter() {
        let term = live.term.lock().await;
        let snapshot = snapshot_term(&term);
        let cols = u16::try_from(term.grid().columns()).unwrap_or(u16::MAX);
        let rows = u16::try_from(term.grid().screen_lines()).unwrap_or(u16::MAX);
        drop(term);

        // Encode as a v5 replay (compressed ANSI). If encoding fails, log and
        // leave session_replay None — the receiver will fall back to the
        // legacy snapshot field and still produce a working session.
        let session_replay = match scribe_common::screen_replay::build_session_replay(&snapshot) {
            Ok(replay) => Some(replay),
            Err(e) => {
                tracing::warn!(%session_id, "build_session_replay failed: {e}");
                None
            }
        };

        let has_ai_state = live.ai_state.is_some();
        tracing::debug!(%session_id, has_ai_state, "serializing live session for handoff");

        handoff_sessions.push(HandoffSession {
            session_id,
            workspace_id: live.workspace_id,
            child_pid: live.child_pid,
            cols,
            rows,
            cell_width: live.cell_width,
            cell_height: live.cell_height,
            snapshot: None,
            session_replay,
            title: Some(live.title.clone()),
            shell_name: live.shell_name.clone(),
            codex_task_label: live.codex_task_label.clone(),
            cwd: live.cwd.clone(),
            context: live.context.clone(),
            ai_state: live.ai_state.clone(),
            ai_provider_hint: live
                .ai_state
                .as_ref()
                .map(|state| state.provider)
                .or(live.ai_provider_hint),
        });

        fds.push(Arc::clone(&live.resize_fd));
    }

    (handoff_sessions, fds)
}

/// Defuse all Pty objects so the old server's exit does not send `SIGHUP` to
/// child processes. Call after a successful handoff, before shutdown.
///
/// `alacritty_terminal::tty::Pty::drop()` explicitly calls
/// `kill(child_pid, SIGHUP)`. Since the new server already holds the PTY
/// master fds (via `SCM_RIGHTS`), the children must stay alive.
/// `std::mem::forget` prevents the `Drop` impl from running.
/// Move all sessions from the `SessionManager` into the live registry and
/// start their PTY reader tasks in detached mode (no client writer).
///
/// Called at the start of `run_server_loop` so that sessions restored from a
/// hot-reload handoff are available for `ListSessions` / `AttachSessions`
/// before any client connects. For a normal (non-upgrade) startup this is a
/// no-op because the `SessionManager` starts empty.
pub async fn activate_pending_sessions(
    session_manager: &SessionManager,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
) {
    let pending = session_manager.pending_session_ids().await;

    for (session_id, workspace_id) in pending {
        if let Some(session) = session_manager.take_session(session_id).await {
            start_session(
                session_id,
                workspace_id,
                session,
                InitialAttachment { writer: None, attached_ids: None },
                SessionRuntimeContext { workspace_manager, live_sessions },
            )
            .await;
            info!(%session_id, "activated restored session (detached)");
        }
    }
}

pub async fn defuse_for_handoff(live_sessions: &LiveSessionRegistry) {
    let mut sessions = live_sessions.write().await;
    for (&session_id, session) in sessions.iter_mut() {
        if let Some(pty) = session.pty.take() {
            // Wrap in ManuallyDrop to prevent Pty::drop() from running.
            // ManuallyDrop does not call the inner type's Drop on scope exit.
            let _defused = std::mem::ManuallyDrop::new(pty);
            info!(%session_id, "defused Pty to prevent SIGHUP on exit");
        }
    }
}

/// Decide which `WindowId` to assign to a connecting client, and which
/// other unconnected windows should be spawned as separate processes.
///
/// When `hello_window_id` is `Some`, the client already knows its ID
/// (e.g. it was launched with `--window-id`). When `None`, this is a
/// fresh launch — if there are unconnected windows with sessions
/// (restart scenario), the client adopts one instead of creating a new ID.
fn resolve_window_assignment<V>(
    hello_window_id: Option<WindowId>,
    windows_with_sessions: &HashSet<WindowId>,
    connected: &HashMap<WindowId, V>,
) -> (WindowId, Vec<WindowId>) {
    let assigned = hello_window_id.unwrap_or_else(|| {
        windows_with_sessions
            .iter()
            .find(|wid| !connected.contains_key(wid))
            .copied()
            .unwrap_or_else(WindowId::new)
    });

    let other_windows: Vec<WindowId> = windows_with_sessions
        .iter()
        .filter(|wid| **wid != assigned && !connected.contains_key(wid))
        .copied()
        .collect();

    (assigned, other_windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::framing::read_message;
    use std::os::unix::net::UnixStream as StdUnixStream;

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().unwrap();
        left.set_nonblocking(true).unwrap();
        right.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(left).unwrap(),
            tokio::net::UnixStream::from_std(right).unwrap(),
        )
    }

    #[tokio::test]
    async fn attach_sessions_returns_empty_when_registry_has_no_matching_sessions() {
        let live_sessions = new_live_session_registry();

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = Arc::new(Mutex::new(write));
        let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

        let attached = crate::attach_flow::attach_sessions(
            &[SessionId::new()],
            &[],
            &live_sessions,
            crate::attach_flow::AttachClientContext {
                writer: &writer,
                attached_ids: &attached_ids,
            },
        )
        .await;

        assert!(attached.is_empty());
    }

    /// Fresh first launch — no prior sessions exist.
    /// Should create a new window ID and no other windows.
    #[test]
    fn fresh_launch_no_sessions_creates_new_window() {
        let sessions: HashSet<WindowId> = HashSet::new();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        // Should get a new (unique) window ID, and no windows to spawn.
        assert!(!sessions.contains(&assigned), "should be a brand-new ID");
        assert!(others.is_empty());
    }

    /// Restart with 1 window — one unconnected window has sessions.
    /// The connecting client should adopt that window, not create a new one.
    #[test]
    fn restart_single_window_reuses_existing() {
        let w1 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_eq!(assigned, w1, "should reuse the existing window");
        assert!(others.is_empty(), "no other windows to spawn");
    }

    /// Restart with multiple windows — client adopts one, rest in `other_windows`.
    #[test]
    fn restart_multi_window_adopts_one_spawns_rest() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let w3 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2, w3].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert!(sessions.contains(&assigned), "should adopt an existing window");
        assert_eq!(others.len(), 2, "should spawn the other 2 windows");
        assert!(!others.contains(&assigned), "assigned must not appear in others");
        for o in &others {
            assert!(sessions.contains(o), "other_windows must be known windows");
        }
    }

    /// Explicit --window-id always used, even if it doesn't match any session.
    #[test]
    fn explicit_window_id_used_as_is() {
        let w1 = WindowId::new();
        let w_explicit = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(Some(w_explicit), &sessions, &connected);

        assert_eq!(assigned, w_explicit, "should use the explicit ID");
        assert_eq!(others, vec![w1], "unconnected session window should be in others");
    }

    /// New window spawned while another is already connected — should not
    /// steal the connected window's ID.
    #[test]
    fn does_not_steal_connected_window() {
        let w1 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = [(w1, true)].into_iter().collect();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_ne!(assigned, w1, "must not steal connected window");
        assert!(others.is_empty(), "w1 is connected so not in others");
    }

    /// Mix of connected and unconnected windows — only adopts an unconnected one.
    #[test]
    fn adopts_unconnected_skips_connected() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2].into_iter().collect();
        let connected: HashMap<WindowId, bool> = [(w1, true)].into_iter().collect();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_eq!(assigned, w2, "should adopt the unconnected window");
        assert!(others.is_empty(), "w1 is connected, w2 is assigned — nothing left");
    }

    /// Explicit window-id that matches a session — no duplication in others.
    #[test]
    fn explicit_id_matching_session_not_in_others() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(Some(w1), &sessions, &connected);

        assert_eq!(assigned, w1);
        assert_eq!(others, vec![w2], "only the other unconnected window");
    }

    #[test]
    fn ai_clear_rewrite_applies_to_claude_and_codex() {
        let claude = AiProcessState::new_with_provider(AiProvider::ClaudeCode, AiState::Processing);
        let codex = AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::Processing);
        let supported_events = [
            MetadataEvent::AiStateChanged(codex.clone()),
            MetadataEvent::AiStateChanged(claude.clone()),
        ];
        let unsupported_events = [MetadataEvent::AiStateCleared];
        let chunk_has_supported = chunk_mentions_ed3_provider(&supported_events);
        let chunk_has_no_supported = chunk_mentions_ed3_provider(&unsupported_events);

        assert!(ai_state_uses_ed3_filter(Some(&claude)));
        assert!(ai_state_uses_ed3_filter(Some(&codex)));
        assert!(!ai_state_uses_ed3_filter(None));
        assert!(chunk_has_supported);
        assert!(!chunk_has_no_supported);
        assert!(should_apply_ed3_filter(None, chunk_has_supported));
        assert!(!should_apply_ed3_filter(None, chunk_has_no_supported));
    }

    #[tokio::test]
    async fn dispatch_action_routes_to_target_and_acknowledges_requester() {
        let connected = new_connected_clients();
        let window_id = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter = Arc::new(Mutex::new(request_write));

        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter = Arc::new(Mutex::new(target_write));

        connected.write().await.insert(window_id, Arc::clone(&target_writer));

        handle_dispatch_action(
            Some(window_id),
            AutomationAction::OpenSettings,
            &connected,
            &request_writer,
        )
        .await;

        let routed: ServerMessage = read_message(&mut target_client).await.unwrap();
        assert!(matches!(
            routed,
            ServerMessage::RunAction { action: AutomationAction::OpenSettings }
        ));

        let ack: ServerMessage = read_message(&mut request_client).await.unwrap();
        assert!(
            matches!(ack, ServerMessage::ActionDispatched { window_id: ack_id } if ack_id == window_id)
        );
    }

    #[tokio::test]
    async fn dispatch_action_reports_missing_window() {
        let connected = new_connected_clients();
        let missing_window = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter = Arc::new(Mutex::new(request_write));

        handle_dispatch_action(
            Some(missing_window),
            AutomationAction::OpenSettings,
            &connected,
            &request_writer,
        )
        .await;

        let response: ServerMessage = read_message(&mut request_client).await.unwrap();
        assert!(
            matches!(response, ServerMessage::Error { message } if message.contains("window not connected"))
        );
    }
}
