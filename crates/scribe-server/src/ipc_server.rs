use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::UnixListener;
use tokio::net::unix::UCred;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};
use vte::Parser as VteParser;
use vte::ansi::Processor as AnsiProcessor;

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::SessionId;
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::socket::current_uid;
use scribe_pty::metadata::MetadataEvent;
use scribe_pty::osc_interceptor::OscInterceptor;

use crate::session_manager::{ManagedSession, SessionManager};
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

/// State needed by the PTY reader task, extracted from `ManagedSession`.
struct PtyReaderState {
    session_id: SessionId,
    pty_read: ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    metadata_parser: scribe_pty::metadata::MetadataParser,
    metadata_rx: tokio::sync::mpsc::UnboundedReceiver<MetadataEvent>,
    writer: SharedWriter,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    /// Reusable buffer for OSC events — cleared between iterations to avoid
    /// allocating a new `Vec` on every PTY read.
    osc_events: Vec<MetadataEvent>,
}

/// State shared between the client handler and PTY reader tasks for a session.
struct SessionHandle {
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    pty_raw_fd: RawFd,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    child_pid: u32,
    /// Keep the Pty alive so the child process isn't killed by SIGHUP on Drop.
    /// `None` for sessions restored from a hot-reload handoff.
    #[allow(dead_code, reason = "must stay alive to prevent child SIGHUP")]
    _pty: Option<alacritty_terminal::tty::Pty>,
}

/// Start the IPC server on the given Unix socket path.
pub async fn start_ipc_server(
    socket_path: &Path,
    session_manager: Arc<SessionManager>,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
) -> Result<(), ScribeError> {
    prepare_socket(socket_path)?;

    let listener = UnixListener::bind(socket_path).map_err(|e| ScribeError::Io { source: e })?;
    let connection_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    info!(?socket_path, "IPC server listening");

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
                tokio::spawn(async move {
                    handle_client(stream, sm, wm).await;
                    drop(permit);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// Remove a stale socket file and set up the parent directory with 0700 permissions.
fn prepare_socket(socket_path: &Path) -> Result<(), ScribeError> {
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(ScribeError::Io { source: e });
        }
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    Ok(())
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

/// Per-client connection handler. Reads `ClientMessage`s and dispatches them.
async fn handle_client(
    stream: tokio::net::UnixStream,
    session_manager: Arc<SessionManager>,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
) {
    let (reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let mut reader = reader;

    // Track sessions owned by this client for cleanup on disconnect.
    let mut session_handles: std::collections::HashMap<SessionId, SessionHandle> =
        std::collections::HashMap::new();

    loop {
        let msg: ClientMessage = match read_message(&mut reader).await {
            Ok(msg) => msg,
            Err(ScribeError::Io { .. }) => {
                debug!("client disconnected");
                break;
            }
            Err(e) => {
                warn!("failed to read client message: {e}");
                break;
            }
        };

        dispatch_message(msg, &session_manager, &workspace_manager, &writer, &mut session_handles)
            .await;
    }

    // Clean up all sessions owned by this client.
    for session_id in session_handles.keys() {
        session_manager.close_session(*session_id).await;
        workspace_manager.write().await.remove_session(*session_id);
    }
}

/// Dispatch a single `ClientMessage` to the appropriate handler.
async fn dispatch_message(
    msg: ClientMessage,
    session_manager: &Arc<SessionManager>,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    session_handles: &mut std::collections::HashMap<SessionId, SessionHandle>,
) {
    match msg {
        ClientMessage::CreateSession { workspace_id } => {
            handle_create_session(
                workspace_id,
                session_manager,
                workspace_manager,
                writer,
                session_handles,
            )
            .await;
        }
        ClientMessage::KeyInput { session_id, data } => {
            handle_key_input(session_id, &data, session_handles).await;
        }
        ClientMessage::CloseSession { session_id } => {
            handle_close_session(session_id, session_manager, workspace_manager, session_handles)
                .await;
        }
        ClientMessage::Resize { session_id, cols, rows } => {
            handle_resize(session_id, cols, rows, session_handles).await;
        }
        ClientMessage::Subscribe { session_ids } => {
            let cap = session_ids.len().min(MAX_SUBSCRIBE_IDS);
            let ids = session_ids.get(..cap).unwrap_or(&session_ids);
            handle_subscribe(ids, workspace_manager, writer, session_handles).await;
        }
        ClientMessage::ConfigReloaded => {
            handle_config_reloaded();
        }
        other => {
            debug!(?other, "unhandled client message");
        }
    }
}

/// Create a new PTY session, take ownership, start the reader task.
async fn handle_create_session(
    workspace_id: scribe_common::ids::WorkspaceId,
    session_manager: &Arc<SessionManager>,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    session_handles: &mut std::collections::HashMap<SessionId, SessionHandle>,
) {
    let session_id = match session_manager.create_session(workspace_id).await {
        Ok(id) => id,
        Err(e) => {
            send_error(writer, &format!("failed to create session: {e}")).await;
            return;
        }
    };

    // Register session with workspace manager.
    {
        let mut wm = workspace_manager.write().await;
        wm.add_session(workspace_id, session_id);
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
        if let Some((name, accent_color)) = wm.workspace_info(workspace_id) {
            let info_msg = ServerMessage::WorkspaceInfo { workspace_id, name, accent_color };
            send_message(writer, &info_msg).await;
        }
    }

    start_session(session_id, session, writer, workspace_manager, session_handles);
}

/// Split a `ManagedSession` and start the PTY reader task.
fn start_session(
    session_id: SessionId,
    session: ManagedSession,
    writer: &SharedWriter,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    session_handles: &mut std::collections::HashMap<SessionId, SessionHandle>,
) {
    // Extract all fields from session before partial moves.
    let raw_fd = session.pty_fd.raw_fd();
    let term = session.term;
    let child_pid = session.child_pid;
    let pty = session.pty;
    let ansi_processor = session.ansi_processor;
    let osc_parser = session.osc_parser;
    let metadata_parser = session.metadata_parser;
    let metadata_rx = session.metadata_rx;

    let (pty_read, pty_write) = tokio::io::split(session.pty_fd);
    let pty_write = Arc::new(Mutex::new(pty_write));

    let handle = SessionHandle {
        pty_write: Arc::clone(&pty_write),
        pty_raw_fd: raw_fd,
        term: Arc::clone(&term),
        child_pid,
        _pty: pty,
    };
    session_handles.insert(session_id, handle);

    let state = PtyReaderState {
        session_id,
        pty_read,
        term,
        ansi_processor,
        osc_parser,
        metadata_parser,
        metadata_rx,
        writer: Arc::clone(writer),
        workspace_manager: Arc::clone(workspace_manager),
        osc_events: Vec::new(),
    };

    tokio::spawn(pty_reader_task(state));
}

/// Write key input data to the PTY.
async fn handle_key_input(
    session_id: SessionId,
    data: &[u8],
    session_handles: &std::collections::HashMap<SessionId, SessionHandle>,
) {
    if data.len() > MAX_KEY_INPUT_BYTES {
        warn!(
            %session_id,
            len = data.len(),
            max = MAX_KEY_INPUT_BYTES,
            "KeyInput payload too large, dropping"
        );
        return;
    }

    let Some(handle) = session_handles.get(&session_id) else {
        warn!(%session_id, "KeyInput for unknown session");
        return;
    };

    let mut pty_write = handle.pty_write.lock().await;
    if let Err(e) = pty_write.write_all(data).await {
        warn!(%session_id, "failed to write to PTY: {e}");
    }
}

/// Close a session and clean up.
async fn handle_close_session(
    session_id: SessionId,
    session_manager: &SessionManager,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    session_handles: &mut std::collections::HashMap<SessionId, SessionHandle>,
) {
    session_handles.remove(&session_id);
    session_manager.close_session(session_id).await;
    workspace_manager.write().await.remove_session(session_id);
    info!(%session_id, "session closed by client");
}

/// Resize the terminal and PTY.
async fn handle_resize(
    session_id: SessionId,
    cols: u16,
    rows: u16,
    session_handles: &std::collections::HashMap<SessionId, SessionHandle>,
) {
    if cols == 0 || rows == 0 {
        warn!(%session_id, cols, rows, "ignoring resize with zero dimension");
        return;
    }

    let Some(handle) = session_handles.get(&session_id) else {
        warn!(%session_id, "Resize for unknown session");
        return;
    };

    // Resize the Term state (lock + drop before any await).
    resize_term(&handle.term, cols, rows).await;

    // Signal the PTY with TIOCSWINSZ.
    if let Err(e) = set_pty_winsize(handle.pty_raw_fd, cols, rows) {
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
    session_handles: &std::collections::HashMap<SessionId, SessionHandle>,
) {
    for &session_id in session_ids {
        let Some(handle) = session_handles.get(&session_id) else {
            continue;
        };

        let msg = {
            let mut wm = workspace_manager.write().await;
            wm.check_cwd_fallback(session_id, handle.child_pid)
        };

        if let Some(named_msg) = msg {
            send_message(writer, &named_msg).await;
        }
    }
}

/// Handle `ConfigReloaded` — reload the config file and log the result.
fn handle_config_reloaded() {
    match crate::config::load_config() {
        Ok(_cfg) => {
            info!("config reloaded successfully via client request");
        }
        Err(e) => {
            warn!("config reload failed: {e}");
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

/// Send an error message to the client.
async fn send_error(writer: &SharedWriter, message: &str) {
    let msg = ServerMessage::Error { message: message.to_owned() };
    send_message(writer, &msg).await;
}

// ── PTY reader task ─────────────────────────────────────────────

/// The dual-path read loop: raw bytes to UI (fast path) + Term state + metadata.
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

        // Step 1: Fast path — forward raw bytes to UI client.
        send_pty_output(&state.writer, state.session_id, bytes).await;

        // Step 2: State path — feed into Term via ANSI processor.
        feed_term(&state.term, &mut state.ansi_processor, bytes).await;

        // Step 3: OSC interceptor for AI state metadata.
        // Reuse the pre-allocated vec to avoid a heap allocation each iteration.
        run_osc_interceptor(
            &mut state.osc_parser,
            &state.metadata_parser,
            bytes,
            &mut state.osc_events,
        );
        // Swap out the events vec so we can drive async sends without holding
        // a borrow on `state.osc_events` across await points, then swap back
        // the (now-empty) vec so its capacity is retained for the next read.
        let mut events_this_iter = std::mem::take(&mut state.osc_events);
        for event in events_this_iter.drain(..) {
            send_metadata_event(event, state.session_id, &state.writer, &state.workspace_manager)
                .await;
        }
        state.osc_events = events_this_iter;

        // Step 4: Drain metadata events from ScribeEventListener channel.
        drain_metadata_events(
            &mut state.metadata_rx,
            state.session_id,
            &state.writer,
            &state.workspace_manager,
        )
        .await;
    }

    // Session EOF — notify client.
    let exit_msg = ServerMessage::SessionExited { session_id: state.session_id, exit_code: None };
    send_message(&state.writer, &exit_msg).await;
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

/// Send raw PTY output to the client (fast path).
async fn send_pty_output(writer: &SharedWriter, session_id: SessionId, bytes: &[u8]) {
    let msg = ServerMessage::PtyOutput { session_id, data: bytes.to_vec() };
    send_message(writer, &msg).await;
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

/// Drain the metadata event channel from `ScribeEventListener` and send events.
async fn drain_metadata_events(
    metadata_rx: &mut tokio::sync::mpsc::UnboundedReceiver<MetadataEvent>,
    session_id: SessionId,
    writer: &SharedWriter,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    while let Ok(event) = metadata_rx.try_recv() {
        send_metadata_event(event, session_id, writer, workspace_manager).await;
    }
}

/// Detect the current git branch by walking up from `cwd` looking for `.git/HEAD`.
///
/// Returns `Some(branch_name)` if on a named branch, `Some(short_sha)` if in
/// detached HEAD state, or `None` if not inside a git repository.
fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    loop {
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

/// Convert a `MetadataEvent` to a `ServerMessage` and send it.
/// For `CwdChanged`, also notifies the workspace manager and sends git branch.
async fn send_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
    writer: &SharedWriter,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    let (server_msg, cwd_for_workspace) = convert_metadata_event(event, session_id);

    send_message(writer, &server_msg).await;

    if let Some(cwd) = cwd_for_workspace {
        // Send git branch information for the new CWD.
        let branch = detect_git_branch(&cwd);
        let git_msg = ServerMessage::GitBranch { session_id, branch };
        send_message(writer, &git_msg).await;

        let named_msg = {
            let mut wm = workspace_manager.write().await;
            wm.on_cwd_changed(session_id, &cwd)
        };
        if let Some(msg) = named_msg {
            send_message(writer, &msg).await;
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
        MetadataEvent::Bell => (ServerMessage::Bell { session_id }, None),
    }
}
