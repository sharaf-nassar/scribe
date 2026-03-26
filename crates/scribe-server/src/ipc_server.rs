use std::collections::{HashMap, HashSet};
use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::UnixListener;
use tokio::net::unix::UCred;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};
use vte::Parser as VteParser;
use vte::ansi::Processor as AnsiProcessor;

use alacritty_terminal::grid::Dimensions as _;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{ClientMessage, ServerMessage, SessionInfo};
use scribe_common::socket::current_uid;
use scribe_pty::metadata::MetadataEvent;
use scribe_pty::osc_interceptor::OscInterceptor;

use crate::handoff::HandoffSession;
use crate::session_manager::{ManagedSession, SessionManager, snapshot_term};
use crate::updater::UpdaterHandle;
use crate::workspace_manager::WorkspaceManager;

/// Buffer size for PTY reads. 64 KiB balances throughput and latency.
const PTY_READ_BUF_SIZE: usize = 64 * 1024;

/// Maximum payload size for a single `KeyInput` message. Legitimate keyboard
/// input is never more than a few dozen bytes; capping at 4 KiB prevents a
/// client from writing 16 MiB (the frame limit) to the PTY in one shot.
const MAX_KEY_INPUT_BYTES: usize = 4 * 1024;

/// Maximum simultaneous IPC client connections. Prevents a same-UID attacker
/// from exhausting memory/tasks by opening thousands of connections.
const MAX_CONNECTIONS: usize = 32;

/// Maximum number of session IDs in a single `Subscribe` message. Prevents
/// a client from holding the workspace write-lock in a tight loop.
const MAX_SUBSCRIBE_IDS: usize = 256;

/// Shared writer half of the client connection.
type SharedWriter = Arc<Mutex<WriteHalf<tokio::net::UnixStream>>>;

/// Optional client writer: `Some` when a client is attached, `None` when
/// the session is detached (client disconnected). The PTY reader task
/// silently skips sends when `None`.
type ClientWriter = Arc<Mutex<Option<SharedWriter>>>;

/// Server-wide registry of all running sessions. Shared across client
/// handlers and the handoff listener — sessions survive client disconnects.
pub type LiveSessionRegistry = Arc<RwLock<HashMap<SessionId, LiveSession>>>;

/// Registry of connected client windows, keyed by `WindowId`.
/// Used to broadcast `QuitRequested` to all connected clients.
pub type ConnectedClients = Arc<RwLock<HashMap<WindowId, SharedWriter>>>;

/// State needed by the PTY reader task, extracted from `ManagedSession`.
struct PtyReaderState {
    session_id: SessionId,
    child_pid: u32,
    pty_read: ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    metadata_parser: scribe_pty::metadata::MetadataParser,
    metadata_rx: tokio::sync::mpsc::UnboundedReceiver<MetadataEvent>,
    client_writer: ClientWriter,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    /// Reusable buffer for OSC events — cleared between iterations to avoid
    /// allocating a new `Vec` on every PTY read.
    osc_events: Vec<MetadataEvent>,
    /// Last known CWD from `/proc/pid/cwd`, used to detect changes triggered
    /// by title-change events (for shells that emit OSC 0 but not OSC 7).
    last_proc_cwd: Option<std::path::PathBuf>,
}

/// A running session in the server-wide registry. Lives independently of
/// any client connection — the `client_writer` is set/cleared as clients
/// attach and detach.
pub struct LiveSession {
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    pty_raw_fd: RawFd,
    pub term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    child_pid: u32,
    client_writer: ClientWriter,
    workspace_id: WorkspaceId,
    /// Last-known terminal title (OSC 0/2), persisted for reconnect.
    title: String,
    /// Last-known working directory (OSC 7), persisted for reconnect.
    cwd: Option<std::path::PathBuf>,
    /// Last-known AI process state (OSC 1337), persisted for reconnect.
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
    /// Keep the Pty alive so the child process isn't killed by SIGHUP on Drop.
    /// `None` for sessions restored from a hot-reload handoff. Taken and leaked
    /// by `defuse_for_handoff` during hot-reload to prevent SIGHUP.
    pty: Option<alacritty_terminal::tty::Pty>,
    /// Screen snapshot from a hot-reload handoff, sent to the first client
    /// that attaches. Taken (cleared) after first use.
    pub handoff_snapshot: Option<scribe_common::screen::ScreenSnapshot>,
}

/// Start the IPC accept loop on an already-bound listener.
#[allow(clippy::too_many_arguments, reason = "IPC server requires all server subsystems")]
pub async fn start_ipc_server(
    listener: UnixListener,
    session_manager: Arc<SessionManager>,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    connected_clients: ConnectedClients,
    updater_handle: Arc<UpdaterHandle>,
) -> Result<(), ScribeError> {
    let connection_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    info!("IPC server listening");

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
                let sm = Arc::clone(&session_manager);
                let wm = Arc::clone(&workspace_manager);
                let ls = Arc::clone(&live_sessions);
                let cc = Arc::clone(&connected_clients);
                let uh = Arc::clone(&updater_handle);
                tokio::spawn(async move {
                    handle_client(stream, sm, wm, ls, cc, uh).await;
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
) -> Result<(Option<std::fs::File>, UnixListener), ScribeError> {
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

    #[allow(
        deprecated,
        reason = "nix::fcntl::Flock requires OwnedFd which conflicts with our File ownership"
    )]
    nix::fcntl::flock(lock_file.as_raw_fd(), nix::fcntl::FlockArg::LockExclusiveNonblock).map_err(
        |_| ScribeError::IpcError {
            reason: "another scribe-server is already running (lock held)".into(),
        },
    )?;

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
#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_arguments,
    reason = "connection handler with handshake + message loop requires all server subsystems"
)]
async fn handle_client(
    stream: tokio::net::UnixStream,
    session_manager: Arc<SessionManager>,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    connected_clients: ConnectedClients,
    updater_handle: Arc<UpdaterHandle>,
) {
    let (reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let mut reader = reader;

    // Track which sessions this client has attached to, for detach on disconnect.
    let mut attached_ids: HashSet<SessionId> = HashSet::new();

    // Perform Hello/Welcome handshake — wait for the first message.
    let window_id = match read_message::<ClientMessage, _>(&mut reader).await {
        Ok(ClientMessage::Hello { window_id }) => {
            let wm = workspace_manager.read().await;
            let all_windows = wm.window_ids_with_sessions();

            // Read connected clients first so we can reuse an unconnected
            // window on fresh-launch restarts (no --window-id).
            let connected = connected_clients.read().await;

            let (assigned, other_windows) =
                resolve_window_assignment(window_id, &all_windows, &connected);
            drop(connected);
            drop(wm);

            // Register this client in the connected clients map.
            connected_clients.write().await.insert(assigned, Arc::clone(&writer));

            let welcome = ServerMessage::Welcome { window_id: assigned, other_windows };
            send_message(&writer, &welcome).await;

            info!(%assigned, "client identified via Hello");
            assigned
        }
        Ok(msg) => {
            // Legacy client — no Hello. Assign a new window and process the
            // message inline.
            let window_id = WindowId::new();
            connected_clients.write().await.insert(window_id, Arc::clone(&writer));
            info!(%window_id, "legacy client (no Hello), assigned window");

            dispatch_message(
                msg,
                &session_manager,
                &workspace_manager,
                &writer,
                &live_sessions,
                &mut attached_ids,
                window_id,
                &connected_clients,
                &updater_handle,
            )
            .await;
            window_id
        }
        Err(ScribeError::Io { .. }) => {
            debug!("client disconnected before Hello");
            return;
        }
        Err(e) => {
            warn!("failed to read Hello message: {e}");
            return;
        }
    };

    loop {
        let msg: ClientMessage = match read_message(&mut reader).await {
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

        dispatch_message(
            msg,
            &session_manager,
            &workspace_manager,
            &writer,
            &live_sessions,
            &mut attached_ids,
            window_id,
            &connected_clients,
            &updater_handle,
        )
        .await;
    }

    // Detach all sessions — clear the writer so the reader task stops
    // forwarding output, but keep the session alive for reconnection.
    detach_sessions(&live_sessions, &attached_ids).await;
    connected_clients.write().await.remove(&window_id);
    info!(%window_id, "client removed from connected clients");
}

/// Clear the client writer for each session so output stops being forwarded.
/// Sessions remain alive in the registry for future client attachment.
async fn detach_sessions(live_sessions: &LiveSessionRegistry, ids: &HashSet<SessionId>) {
    let sessions = live_sessions.read().await;
    for id in ids {
        if let Some(session) = sessions.get(id) {
            *session.client_writer.lock().await = None;
            info!(%id, "session detached (client disconnected)");
        }
    }
}

/// Dispatch a single `ClientMessage` to the appropriate handler.
#[allow(
    clippy::too_many_arguments,
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "dispatch hub — all handler dependencies are passed through"
)]
async fn dispatch_message(
    msg: ClientMessage,
    session_manager: &Arc<SessionManager>,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &mut HashSet<SessionId>,
    window_id: WindowId,
    connected_clients: &ConnectedClients,
    updater_handle: &UpdaterHandle,
) {
    match msg {
        ClientMessage::CreateSession { workspace_id, split_direction, cwd } => {
            handle_create_session(
                workspace_id,
                split_direction,
                cwd,
                session_manager,
                workspace_manager,
                writer,
                live_sessions,
                attached_ids,
                window_id,
            )
            .await;
        }
        ClientMessage::KeyInput { session_id, data } => {
            handle_key_input(session_id, &data, live_sessions, attached_ids).await;
        }
        ClientMessage::CloseSession { session_id } => {
            handle_close_session(session_id, workspace_manager, live_sessions, attached_ids).await;
            workspace_manager.write().await.remove_session_from_window(session_id);
        }
        ClientMessage::Resize { session_id, cols, rows } => {
            handle_resize(session_id, cols, rows, live_sessions, attached_ids).await;
        }
        ClientMessage::Subscribe { session_ids } => {
            let cap = session_ids.len().min(MAX_SUBSCRIBE_IDS);
            let ids = session_ids.get(..cap).unwrap_or(&session_ids);
            handle_subscribe(ids, workspace_manager, writer, live_sessions).await;
        }
        ClientMessage::RequestSnapshot { session_id } => {
            handle_request_snapshot(session_id, writer, live_sessions).await;
        }
        ClientMessage::CreateWorkspace => {
            handle_create_workspace(workspace_manager, writer).await;
        }
        ClientMessage::ListSessions => {
            handle_list_sessions(live_sessions, workspace_manager, writer, window_id).await;
        }
        ClientMessage::AttachSessions { session_ids } => {
            handle_attach_sessions(
                &session_ids,
                live_sessions,
                workspace_manager,
                writer,
                attached_ids,
            )
            .await;
        }
        ClientMessage::ConfigReloaded => {
            handle_config_reloaded(session_manager, live_sessions).await;
        }
        ClientMessage::ReportWorkspaceTree { tree } => {
            debug!(%window_id, "received workspace tree from client");
            let mut wm = workspace_manager.write().await;
            wm.set_workspace_tree(tree.clone());
            wm.set_window_tree(window_id, tree);
        }
        ClientMessage::Hello { .. } => {
            // Hello is handled during the handshake phase, not here.
            debug!("unexpected Hello after handshake, ignoring");
        }
        ClientMessage::CloseWindow { window_id: target_window } => {
            handle_close_window(target_window, workspace_manager, live_sessions, attached_ids)
                .await;
        }
        ClientMessage::QuitAll => {
            handle_quit_all(window_id, connected_clients).await;
        }
        ClientMessage::TriggerUpdate => {
            info!(%window_id, "client triggered update");
            updater_handle.trigger();
        }
        ClientMessage::DismissUpdate => {
            info!(%window_id, "client dismissed update notification");
            updater_handle.dismiss();
        }
        other => {
            debug!(?other, "unhandled client message");
        }
    }
}

/// Create a new PTY session, register it, start the reader task.
#[allow(
    clippy::too_many_arguments,
    reason = "session creation requires access to all server subsystems"
)]
async fn handle_create_session(
    workspace_id: WorkspaceId,
    split_direction: Option<scribe_common::protocol::LayoutDirection>,
    cwd: Option<std::path::PathBuf>,
    session_manager: &Arc<SessionManager>,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &mut HashSet<SessionId>,
    window_id: WindowId,
) {
    let session_id = match session_manager.create_session(workspace_id, cwd).await {
        Ok(id) => id,
        Err(e) => {
            send_error(writer, &format!("failed to create session: {e}")).await;
            return;
        }
    };

    // Register session with workspace manager.  When `split_direction` is
    // `Some` the workspace is auto-created (client just split the window).
    {
        let mut wm = workspace_manager.write().await;
        wm.add_session(workspace_id, session_id, split_direction);
        wm.assign_session_to_window(window_id, session_id);
    }

    let Some(session) = session_manager.take_session(session_id).await else {
        send_error(writer, "session vanished after creation").await;
        return;
    };

    // Notify client of session creation.
    let creation_msg = ServerMessage::SessionCreated {
        session_id,
        workspace_id,
        shell_name: String::from("shell"),
    };
    send_message(writer, &creation_msg).await;

    // Send workspace info so the client knows the accent color and name.
    {
        let wm = workspace_manager.read().await;
        if let Some((name, accent_color, ws_split_dir)) = wm.workspace_info(workspace_id) {
            let info_msg = ServerMessage::WorkspaceInfo {
                workspace_id,
                name,
                accent_color,
                split_direction: ws_split_dir,
            };
            send_message(writer, &info_msg).await;
        }
    }

    start_session(
        session_id,
        workspace_id,
        session,
        Some(writer),
        workspace_manager,
        live_sessions,
    )
    .await;
    attached_ids.insert(session_id);
}

/// Split a `ManagedSession`, register in the live registry, and start
/// the PTY reader task. When `writer` is `None` the session starts in
/// detached mode (PTY reader runs but output is silently discarded until
/// a client attaches).
///
/// The registry insert is performed synchronously (before the PTY reader
/// task is spawned) to eliminate the race where `CloseSession` could arrive
/// before the session is visible in the registry.
#[allow(
    clippy::too_many_arguments,
    reason = "session startup wires together all subsystem references"
)]
async fn start_session(
    session_id: SessionId,
    workspace_id: WorkspaceId,
    session: ManagedSession,
    writer: Option<&SharedWriter>,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
) {
    // Extract all fields from session before partial moves.
    let raw_fd = session.pty_fd.raw_fd();
    let term = session.term;
    let child_pid = session.child_pid;
    let pty = session.pty;
    let handoff_snapshot = session.handoff_snapshot;
    let ansi_processor = session.ansi_processor;
    let osc_parser = session.osc_parser;
    let metadata_parser = session.metadata_parser;
    let metadata_rx = session.metadata_rx;
    let title = session.title.unwrap_or_else(|| String::from("shell"));
    let cwd = session.cwd;
    let ai_state = session.ai_state;

    let (pty_read, pty_write) = tokio::io::split(session.pty_fd);
    let pty_write = Arc::new(Mutex::new(pty_write));

    // Wrap the client writer in an optional so the reader task can
    // continue running when the client disconnects.
    let client_writer: ClientWriter = Arc::new(Mutex::new(writer.map(Arc::clone)));

    let live = LiveSession {
        pty_write: Arc::clone(&pty_write),
        pty_raw_fd: raw_fd,
        term: Arc::clone(&term),
        child_pid,
        client_writer: Arc::clone(&client_writer),
        workspace_id,
        title,
        cwd,
        ai_state,
        pty,
        handoff_snapshot,
    };

    // Insert into the registry before spawning the PTY reader task so that
    // any concurrent `CloseSession` message sees the session immediately.
    live_sessions.write().await.insert(session_id, live);

    let state = PtyReaderState {
        session_id,
        child_pid,
        pty_read,
        term,
        ansi_processor,
        osc_parser,
        metadata_parser,
        metadata_rx,
        client_writer,
        workspace_manager: Arc::clone(workspace_manager),
        live_sessions: Arc::clone(live_sessions),
        osc_events: Vec::new(),
        last_proc_cwd: None,
    };

    tokio::spawn(pty_reader_task(state));
}

/// Write key input data to the PTY.
async fn handle_key_input(
    session_id: SessionId,
    data: &[u8],
    live_sessions: &LiveSessionRegistry,
    attached_ids: &HashSet<SessionId>,
) {
    if !attached_ids.contains(&session_id) {
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

    let sessions = live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        warn!(%session_id, "KeyInput for unknown session");
        return;
    };

    let mut pty_write = session.pty_write.lock().await;
    if let Err(e) = pty_write.write_all(data).await {
        warn!(%session_id, "failed to write to PTY: {e}");
    }
}

/// Close a session and clean up. Dropping the `LiveSession` sends SIGHUP
/// to the child process and the PTY reader task exits on EOF.
async fn handle_close_session(
    session_id: SessionId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &mut HashSet<SessionId>,
) {
    if !attached_ids.contains(&session_id) {
        tracing::warn!(%session_id, "client sent CloseSession for unattached session");
        return;
    }

    live_sessions.write().await.remove(&session_id);
    workspace_manager.write().await.remove_session(session_id);
    attached_ids.remove(&session_id);
    info!(%session_id, "session closed by client");
}

/// Close a window: destroy every session it owns and remove the window from
/// the workspace manager so it won't be resurrected on the next client launch.
async fn handle_close_window(
    window_id: WindowId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &mut HashSet<SessionId>,
) {
    let session_ids = workspace_manager.read().await.sessions_for_window(window_id);
    info!(%window_id, count = session_ids.len(), "closing window — destroying sessions");

    // Destroy each session (drops PTY fd → SIGHUP → child exit).
    {
        let mut sessions = live_sessions.write().await;
        for &sid in &session_ids {
            sessions.remove(&sid);
            attached_ids.remove(&sid);
        }
    }

    // Remove window and all session→window mappings.
    let mut wm = workspace_manager.write().await;
    for &sid in &session_ids {
        wm.remove_session(sid);
    }
    wm.remove_window(window_id);
}

/// Resize the terminal and PTY.
async fn handle_resize(
    session_id: SessionId,
    cols: u16,
    rows: u16,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &HashSet<SessionId>,
) {
    if !attached_ids.contains(&session_id) {
        tracing::warn!(%session_id, "client sent Resize for unattached session");
        return;
    }

    if cols == 0 || rows == 0 {
        warn!(%session_id, cols, rows, "ignoring resize with zero dimension");
        return;
    }

    let sessions = live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        warn!(%session_id, "Resize for unknown session");
        return;
    };

    // Clone the Arc refs so we can drop the registry lock before awaiting.
    let term = Arc::clone(&session.term);
    let raw_fd = session.pty_raw_fd;
    drop(sessions);

    // Resize the Term state (lock + drop before any await).
    resize_term(&term, cols, rows).await;

    // Signal the PTY with TIOCSWINSZ.
    if let Err(e) = set_pty_winsize(raw_fd, cols, rows) {
        warn!(%session_id, "TIOCSWINSZ failed: {e}");
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
async fn resize_term(
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
/// Writes a `libc::winsize` to the PTY fd, which causes the kernel to send
/// `SIGWINCH` to the foreground process group.
fn set_pty_winsize(fd: RawFd, cols: u16, rows: u16) -> Result<(), ScribeError> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };

    // SAFETY: `fd` is a valid, open PTY master file descriptor. The `winsize`
    // struct is fully initialized and lives on the stack for the duration of
    // the ioctl call. `TIOCSWINSZ` writes a `winsize` to the kernel.
    #[allow(unsafe_code, reason = "TIOCSWINSZ ioctl requires unsafe libc call")]
    let ret = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };

    if ret == -1 {
        return Err(ScribeError::Io { source: std::io::Error::last_os_error() });
    }

    Ok(())
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
    let (name, accent_color, split_direction) =
        wm.workspace_info(workspace_id).unwrap_or_else(|| (None, String::from("#a78bfa"), None));
    drop(wm);

    let msg = ServerMessage::WorkspaceInfo { workspace_id, name, accent_color, split_direction };
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

    let infos: Vec<SessionInfo> = if has_window_sessions {
        // Return only this window's sessions.
        window_session_ids
            .iter()
            .filter_map(|&sid| {
                sessions.get(&sid).map(|s| SessionInfo {
                    session_id: sid,
                    workspace_id: s.workspace_id,
                    title: Some(s.title.clone()),
                    cwd: s.cwd.clone(),
                    ai_state: s.ai_state.clone(),
                })
            })
            .collect()
    } else {
        // No window-specific sessions — return all unowned sessions (legacy
        // fallback or first-time connect with existing sessions).
        sessions
            .iter()
            .filter(|&(&sid, _)| wm.window_for_session(sid).is_none())
            .map(|(&id, s)| SessionInfo {
                session_id: id,
                workspace_id: s.workspace_id,
                title: Some(s.title.clone()),
                cwd: s.cwd.clone(),
                ai_state: s.ai_state.clone(),
            })
            .collect()
    };

    let workspace_ids: Vec<WorkspaceId> = infos.iter().map(|i| i.workspace_id).collect();
    let workspace_tree = wm.window_tree(window_id).cloned();
    drop(wm);
    drop(sessions);

    let list_msg = ServerMessage::SessionList { sessions: infos, workspace_tree };
    send_message(writer, &list_msg).await;

    // Also send workspace info for each referenced workspace so the client
    // can reconstruct the layout (names, accent colours).
    let wm_guard = workspace_manager.read().await;
    let mut seen = HashSet::new();
    for wid in workspace_ids {
        if seen.insert(wid) {
            if let Some((name, accent_color, split_direction)) = wm_guard.workspace_info(wid) {
                let msg = ServerMessage::WorkspaceInfo {
                    workspace_id: wid,
                    name,
                    accent_color,
                    split_direction,
                };
                send_message(writer, &msg).await;
            }
        }
    }
}

/// Take the handoff snapshot if one exists, otherwise snapshot the live Term.
///
/// The handoff snapshot captures the exact pre-handoff screen (including cursor
/// visibility). For just-restored sessions the live Term may be blank, so the
/// handoff snapshot is strongly preferred.
pub async fn take_session_snapshot(
    session_id: SessionId,
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    live_sessions: &LiveSessionRegistry,
) -> scribe_common::screen::ScreenSnapshot {
    let handoff_snap = {
        let mut registry = live_sessions.write().await;
        registry.get_mut(&session_id).and_then(|s| s.handoff_snapshot.take())
    };

    if let Some(snap) = handoff_snap {
        snap
    } else {
        let term_guard = term.lock().await;
        snapshot_term(&term_guard)
    }
}

/// Data extracted from a `LiveSession` for reattachment, collected while
/// holding the registry lock and consumed after releasing it.
struct AttachEntry {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    client_writer: ClientWriter,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    title: String,
    cwd: Option<std::path::PathBuf>,
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
}

/// Handle `AttachSessions` — take ownership of detached sessions, set the
/// client writer, and send back session + workspace info for each.
async fn handle_attach_sessions(
    session_ids: &[SessionId],
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    attached_ids: &mut HashSet<SessionId>,
) {
    let sessions = live_sessions.read().await;

    // Collect data we need, then drop the registry lock before sending.
    let mut attach_data: Vec<AttachEntry> = Vec::new();
    for &session_id in session_ids {
        if let Some(session) = sessions.get(&session_id) {
            attach_data.push(AttachEntry {
                session_id,
                workspace_id: session.workspace_id,
                client_writer: Arc::clone(&session.client_writer),
                term: Arc::clone(&session.term),
                title: session.title.clone(),
                cwd: session.cwd.clone(),
                ai_state: session.ai_state.clone(),
            });
        } else {
            warn!(%session_id, "AttachSessions: session not found");
        }
    }
    drop(sessions);

    for entry in attach_data {
        attach_one_session(&entry, writer, live_sessions, workspace_manager).await;
        attached_ids.insert(entry.session_id);
    }
}

/// Set the client writer on one session, send `SessionCreated`, workspace info,
/// stored metadata, and a screen snapshot to the attaching client.
///
/// The writer is set **after** the snapshot is sent to prevent the PTY reader
/// task from racing live `PtyOutput` against the snapshot (ghost cursors).
///
/// Warns when overwriting an existing writer — this indicates a second client
/// is trying to attach to an already-attached session.
async fn attach_one_session(
    entry: &AttachEntry,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let session_id = entry.session_id;

    // Send SessionCreated so the client can process it through the normal flow.
    let creation_msg = ServerMessage::SessionCreated {
        session_id,
        workspace_id: entry.workspace_id,
        shell_name: String::from("shell"),
    };
    send_message(writer, &creation_msg).await;

    // Send stored metadata so tabs display correctly on reconnect.
    send_stored_metadata(
        writer,
        session_id,
        &entry.title,
        entry.cwd.as_ref(),
        entry.ai_state.as_ref(),
    )
    .await;

    // Send workspace info.
    {
        let wm = workspace_manager.read().await;
        if let Some((name, accent_color, split_direction)) = wm.workspace_info(entry.workspace_id) {
            let msg = ServerMessage::WorkspaceInfo {
                workspace_id: entry.workspace_id,
                name,
                accent_color,
                split_direction,
            };
            send_message(writer, &msg).await;
        }
    }

    let snapshot = take_session_snapshot(session_id, &entry.term, live_sessions).await;
    let snap_msg = ServerMessage::ScreenSnapshot { session_id, snapshot };
    send_message(writer, &snap_msg).await;

    // Set the writer AFTER the snapshot is sent.  The PTY reader task
    // checks this writer on every read — while it is `None`, output
    // is silently dropped (the Term state is still updated).  By
    // deferring the set, we guarantee the client receives the
    // ScreenSnapshot before any live PtyOutput, preventing stale
    // terminal state (cursor position, alt-screen mode) from racing
    // with the snapshot and producing ghost cursors on reconnect.
    let mut cw = entry.client_writer.lock().await;
    if cw.is_some() {
        warn!(
            %session_id,
            "AttachSessions: overwriting existing client writer — \
             previous client may still be connected"
        );
    }
    *cw = Some(Arc::clone(writer));
    drop(cw);

    info!(%session_id, "session attached to new client");
}

/// Send stored title, CWD, git branch, and AI state metadata for a reattached session.
async fn send_stored_metadata(
    writer: &SharedWriter,
    session_id: SessionId,
    title: &str,
    cwd: Option<&std::path::PathBuf>,
    ai_state: Option<&scribe_common::ai_state::AiProcessState>,
) {
    if title != "shell" {
        let title_msg = ServerMessage::TitleChanged { session_id, title: title.to_owned() };
        send_message(writer, &title_msg).await;
    }
    if let Some(cwd) = cwd {
        let cwd_msg = ServerMessage::CwdChanged { session_id, cwd: cwd.clone() };
        send_message(writer, &cwd_msg).await;
        let branch = detect_git_branch(cwd);
        let git_msg = ServerMessage::GitBranch { session_id, branch };
        send_message(writer, &git_msg).await;
    }
    if let Some(ai) = ai_state {
        let ai_msg = ServerMessage::AiStateChanged { session_id, ai_state: ai.clone() };
        send_message(writer, &ai_msg).await;
    }
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

    let term_config = alacritty_terminal::term::Config {
        scrolling_history: new_scrollback,
        ..alacritty_terminal::term::Config::default()
    };
    let sessions = live_sessions.read().await;
    for session in sessions.values() {
        session.term.lock().await.set_options(term_config.clone());
    }
    info!(
        scrollback_lines = new_scrollback,
        sessions = sessions.len(),
        "scrollback updated on live sessions"
    );
}

/// Broadcast `QuitRequested` to all connected clients except the sender.
async fn handle_quit_all(sender_window_id: WindowId, connected_clients: &ConnectedClients) {
    info!(%sender_window_id, "QuitAll requested — broadcasting QuitRequested");
    let clients = connected_clients.read().await;
    let quit_msg = ServerMessage::QuitRequested;
    for (&wid, writer) in &*clients {
        if wid != sender_window_id {
            send_message(writer, &quit_msg).await;
        }
    }
}

/// Send a `ServerMessage` to the client, logging errors.
async fn send_message(writer: &SharedWriter, msg: &ServerMessage) {
    let mut w = writer.lock().await;
    if let Err(e) = write_message(&mut *w, msg).await {
        warn!("failed to send message to client: {e}");
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
        let bytes_read = match read_pty_bytes(&mut state.pty_read, &mut buf).await {
            ReadResult::Data(n) => n,
            ReadResult::Eof => break,
            ReadResult::Err(e) => {
                warn!(session_id = %state.session_id, "PTY read error: {e}");
                break;
            }
        };

        let Some(bytes) = buf.get(..bytes_read) else { break };

        // Step 1: Fast path — forward raw bytes to UI client (no-op if detached).
        send_pty_output(&state.client_writer, state.session_id, bytes).await;

        // Step 2: State path — feed into Term via ANSI processor (always runs).
        feed_term(&state.term, &mut state.ansi_processor, bytes).await;

        // Steps 3–5: Extract, classify, and dispatch metadata events.
        process_metadata_events(&mut state, bytes).await;
    }

    // Session EOF — notify client (if attached) and remove from registry.
    let exit_msg = ServerMessage::SessionExited { session_id: state.session_id, exit_code: None };
    send_to_client(&state.client_writer, &exit_msg).await;
    state.live_sessions.write().await.remove(&state.session_id);
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

/// Parse OSC sequences from bytes using the interceptor. Pure computation, no async.
///
/// Events are pushed into `out`, which the caller clears between iterations to
/// avoid allocating a new `Vec` on every PTY read.
fn run_osc_interceptor(
    osc_parser: &mut VteParser,
    metadata_parser: &scribe_pty::metadata::MetadataParser,
    bytes: &[u8],
    out: &mut Vec<MetadataEvent>,
) {
    let mut interceptor = OscInterceptor::new(metadata_parser, out);
    osc_parser.advance(&mut interceptor, bytes);
}

/// Run the OSC interceptor, drain the metadata channel, classify events,
/// and — if a title changed but no OSC 7 arrived — fall back to
/// `/proc/pid/cwd` for CWD detection.
async fn process_metadata_events(state: &mut PtyReaderState, bytes: &[u8]) {
    let mut saw_title_change = false;
    let mut saw_cwd_change = false;

    // OSC interceptor events.
    run_osc_interceptor(
        &mut state.osc_parser,
        &state.metadata_parser,
        bytes,
        &mut state.osc_events,
    );
    let mut events_this_iter = std::mem::take(&mut state.osc_events);
    for event in events_this_iter.drain(..) {
        classify_event(
            &event,
            &mut saw_title_change,
            &mut saw_cwd_change,
            &mut state.last_proc_cwd,
        );
        send_metadata_event(
            event,
            state.session_id,
            &state.client_writer,
            &state.workspace_manager,
            &state.live_sessions,
        )
        .await;
    }
    state.osc_events = events_this_iter;

    // ScribeEventListener channel events.
    while let Ok(event) = state.metadata_rx.try_recv() {
        classify_event(
            &event,
            &mut saw_title_change,
            &mut saw_cwd_change,
            &mut state.last_proc_cwd,
        );
        send_metadata_event(
            event,
            state.session_id,
            &state.client_writer,
            &state.workspace_manager,
            &state.live_sessions,
        )
        .await;
    }

    // Fallback: title changed but no OSC 7 → read /proc/pid/cwd.
    if saw_title_change && !saw_cwd_change {
        check_proc_cwd(state).await;
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
    let Some(cwd) = macos_proc_cwd(state.child_pid) else {
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

/// Query the CWD of a process on macOS via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
#[cfg(target_os = "macos")]
fn macos_proc_cwd(child_pid: u32) -> Option<std::path::PathBuf> {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::os::raw::c_void;

    const PROC_PIDVNODEPATHINFO: i32 = 9;

    // `proc_vnodepathinfo` is 2 * `vnode_info_path` (each 1152 bytes) = 2304 bytes.
    // `vnode_info_path` = `vnode_info` (128 bytes) + path `[c_char; 1024]`.
    // `pvi_cdir` is the first `vnode_info_path` member; its path starts at byte 128.
    const VIP_PATH_OFFSET: usize = 128;
    const VNODE_INFO_PATH_SIZE: usize = 1152;
    const PROC_VNODEPATHINFO_SIZE: usize = VNODE_INFO_PATH_SIZE * 2;

    #[allow(unsafe_code, reason = "proc_pidinfo FFI is required for macOS CWD detection")]
    {
        unsafe extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut c_void,
                buffersize: i32,
            ) -> i32;
        }

        let mut buf = MaybeUninit::<[u8; PROC_VNODEPATHINFO_SIZE]>::uninit();

        let ret = unsafe {
            proc_pidinfo(
                i32::try_from(child_pid).ok()?,
                PROC_PIDVNODEPATHINFO,
                0,
                buf.as_mut_ptr().cast::<c_void>(),
                i32::try_from(PROC_VNODEPATHINFO_SIZE).ok()?,
            )
        };

        if ret <= 0 {
            return None;
        }

        let buf = unsafe { buf.assume_init() };

        // `pvi_cdir.vip_path` starts at VIP_PATH_OFFSET within the first
        // `vnode_info_path` member. Max path length is 1024 bytes (MAXPATHLEN).
        let path_bytes = buf.get(VIP_PATH_OFFSET..VNODE_INFO_PATH_SIZE)?;

        let c_str = CStr::from_bytes_until_nul(path_bytes).ok()?;
        let path = std::path::PathBuf::from(c_str.to_str().ok()?);

        if path.as_os_str().is_empty() {
            return None;
        }

        Some(path)
    }
}

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
fn detect_git_branch(cwd: &Path) -> Option<String> {
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
        ServerMessage::TitleChanged { title, .. } => {
            if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
                title.clone_into(&mut session.title);
            }
        }
        ServerMessage::CwdChanged { cwd, .. } => {
            if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
                session.cwd = Some(cwd.clone());
            }
        }
        ServerMessage::AiStateChanged { ai_state, .. } => {
            if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
                session.ai_state = Some(ai_state.clone());
            }
        }
        ServerMessage::AiStateCleared { .. } => {
            if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
                session.ai_state = None;
            }
        }
        _ => {}
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

/// Convert a `MetadataEvent` to a `ServerMessage`.
/// Returns the message and optionally the CWD path for workspace naming.
fn convert_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
) -> (ServerMessage, Option<std::path::PathBuf>) {
    match event {
        MetadataEvent::CwdChanged(cwd) => {
            let msg = ServerMessage::CwdChanged { session_id, cwd: cwd.clone() };
            (msg, Some(cwd))
        }
        MetadataEvent::TitleChanged(title) => {
            (ServerMessage::TitleChanged { session_id, title }, None)
        }
        MetadataEvent::AiStateChanged(ai_state) => {
            (ServerMessage::AiStateChanged { session_id, ai_state }, None)
        }
        MetadataEvent::AiStateCleared => (ServerMessage::AiStateCleared { session_id }, None),
        MetadataEvent::Bell => (ServerMessage::Bell { session_id }, None),
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
/// Returns `(sessions, raw_fds)` where the fds are in the same order as the
/// session vec. The caller must send these fds via `SCM_RIGHTS`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "terminal dimensions are always within u16 range"
)]
pub async fn serialize_live_for_handoff(
    live_sessions: &LiveSessionRegistry,
) -> (Vec<HandoffSession>, Vec<RawFd>) {
    let sessions = live_sessions.read().await;
    let mut handoff_sessions = Vec::with_capacity(sessions.len());
    let mut fds = Vec::with_capacity(sessions.len());

    for (&session_id, live) in sessions.iter() {
        let term = live.term.lock().await;
        let snapshot = Some(snapshot_term(&term));
        let cols = term.grid().columns() as u16;
        let rows = term.grid().screen_lines() as u16;
        drop(term);

        handoff_sessions.push(HandoffSession {
            session_id,
            workspace_id: live.workspace_id,
            child_pid: live.child_pid,
            cols,
            rows,
            snapshot,
            title: Some(live.title.clone()),
            cwd: live.cwd.clone(),
            ai_state: live.ai_state.clone(),
        });

        fds.push(live.pty_raw_fd);
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
                None,
                workspace_manager,
                live_sessions,
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
#[allow(
    clippy::zero_sized_map_values,
    reason = "tests use HashMap<WindowId, ()> to match the production connected_clients type"
)]
mod tests {
    use super::*;

    /// Fresh first launch — no prior sessions exist.
    /// Should create a new window ID and no other windows.
    #[test]
    fn fresh_launch_no_sessions_creates_new_window() {
        let sessions: HashSet<WindowId> = HashSet::new();
        let connected: HashMap<WindowId, ()> = HashMap::new();

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
        let connected: HashMap<WindowId, ()> = HashMap::new();

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
        let connected: HashMap<WindowId, ()> = HashMap::new();

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
        let connected: HashMap<WindowId, ()> = HashMap::new();

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
        let connected: HashMap<WindowId, ()> = [(w1, ())].into_iter().collect();

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
        let connected: HashMap<WindowId, ()> = [(w1, ())].into_iter().collect();

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
        let connected: HashMap<WindowId, ()> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(Some(w1), &sessions, &connected);

        assert_eq!(assigned, w1);
        assert_eq!(others, vec![w2], "only the other unconnected window");
    }
}
