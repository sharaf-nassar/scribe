//! Persistent daemon that holds an IPC connection to scribe-server,
//! buffers per-session state, and serves subcommand requests over a local
//! Unix socket.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::screen::ScreenSnapshot;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::cmd_socket::{DaemonRequest, DaemonResponse, daemon_socket_path};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum bytes retained per session output ring buffer.
const MAX_OUTPUT_BUFFER: usize = 65_536;

/// Polling interval for wait loops.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Timeout waiting for the daemon socket to appear after spawning.
const START_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// Whether a session is still running or has exited.
#[derive(Debug, Clone)]
enum SessionStatus {
    Running,
    Exited(Option<i32>),
}

/// Per-session state buffered by the daemon.
#[derive(Debug)]
struct SessionState {
    output_buffer: VecDeque<u8>,
    latest_snapshot: Option<ScreenSnapshot>,
    /// When the latest snapshot was received, for cache freshness checks.
    snapshot_time: Option<tokio::time::Instant>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    status: SessionStatus,
}

impl SessionState {
    fn new() -> Self {
        Self {
            output_buffer: VecDeque::with_capacity(MAX_OUTPUT_BUFFER),
            latest_snapshot: None,
            snapshot_time: None,
            cwd: None,
            title: None,
            status: SessionStatus::Running,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared daemon state
// ---------------------------------------------------------------------------

/// Shared state accessible by both the server reader and request handlers.
struct DaemonState {
    sessions: HashMap<SessionId, SessionState>,
    /// Last workspace ID received from a `WorkspaceInfo` message.
    last_workspace_id: Option<WorkspaceId>,
    /// Last session ID received from a `SessionCreated` message.
    last_session_created: Option<SessionId>,
}

impl DaemonState {
    fn new() -> Self {
        Self { sessions: HashMap::new(), last_workspace_id: None, last_session_created: None }
    }
}

/// Notification channels used to wake up waiting request handlers.
struct WaitNotifiers {
    output: Arc<Notify>,
    cwd: Arc<Notify>,
    exit: Arc<Notify>,
    workspace_info: Arc<Notify>,
    session_created: Arc<Notify>,
}

impl WaitNotifiers {
    fn new() -> Self {
        Self {
            output: Arc::new(Notify::new()),
            cwd: Arc::new(Notify::new()),
            exit: Arc::new(Notify::new()),
            workspace_info: Arc::new(Notify::new()),
            session_created: Arc::new(Notify::new()),
        }
    }
}

type SharedState = Arc<Mutex<DaemonState>>;

// ---------------------------------------------------------------------------
// Daemon lifecycle: start / run / stop
// ---------------------------------------------------------------------------

/// Spawn the daemon as a background child process, then wait for its socket.
pub async fn start() -> Result<(), ScribeError> {
    let exe = std::env::current_exe().map_err(|e| ScribeError::IpcError {
        reason: format!("failed to resolve own executable: {e}"),
    })?;

    std::process::Command::new(exe)
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ScribeError::IpcError { reason: format!("failed to spawn daemon: {e}") })?;

    wait_for_daemon_socket().await
}

/// Poll until the daemon socket appears on disk.
async fn wait_for_daemon_socket() -> Result<(), ScribeError> {
    let path = daemon_socket_path();
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;

    loop {
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ScribeError::IpcError {
                reason: format!("timed out waiting for daemon socket at {}", path.display()),
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Connect to the daemon and send a shutdown request.
pub async fn stop() -> Result<(), ScribeError> {
    let request = DaemonRequest::Shutdown;
    let path = daemon_socket_path();
    let stream = UnixStream::connect(&path).await.map_err(|e| ScribeError::IpcError {
        reason: format!("failed to connect to daemon: {e}"),
    })?;

    let (mut reader, mut writer) = stream.into_split();
    write_message(&mut writer, &request).await?;

    // Wait for the response (daemon will exit after responding).
    let _response: DaemonResponse = read_message(&mut reader).await?;
    Ok(())
}

/// Run the daemon event loop (foreground). This is the `daemon run` entry.
pub async fn run() -> Result<(), ScribeError> {
    let state: SharedState = Arc::new(Mutex::new(DaemonState::new()));
    let notifiers = Arc::new(WaitNotifiers::new());

    let server_conn = crate::ipc::connect().await?;
    let (server_reader, server_writer) = server_conn.into_split();
    let server_writer = Arc::new(Mutex::new(server_writer));

    let socket_path = daemon_socket_path();
    cleanup_stale_socket(&socket_path).await;
    let listener = bind_daemon_socket(&socket_path)?;

    let shutdown = Arc::new(Notify::new());

    let reader_handle =
        tokio::spawn(server_reader_loop(server_reader, Arc::clone(&state), Arc::clone(&notifiers)));

    let listener_handle = tokio::spawn(command_listener_loop(
        listener,
        Arc::clone(&state),
        Arc::clone(&notifiers),
        Arc::clone(&server_writer),
        Arc::clone(&shutdown),
    ));

    let shutdown_handle = tokio::spawn({
        let shutdown = Arc::clone(&shutdown);
        async move { shutdown.notified().await }
    });

    // Wait for shutdown signal or task failure.
    tokio::select! {
        () = async { shutdown_handle.await.ok(); } => {
            info!("daemon shutting down");
        }
        result = reader_handle => {
            if let Ok(Err(e)) = result {
                warn!("server reader ended: {e}");
            }
        }
    }

    cleanup_socket(&socket_path).await;
    listener_handle.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket helpers
// ---------------------------------------------------------------------------

/// Remove a stale socket file if it exists.
async fn cleanup_stale_socket(path: &PathBuf) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "best-effort removal of stale socket; ignore errors"
    )]
    let _ = tokio::fs::remove_file(path).await;
}

/// Remove the daemon socket on shutdown.
async fn cleanup_socket(path: &PathBuf) {
    #[allow(clippy::let_underscore_must_use, reason = "best-effort cleanup on shutdown")]
    let _ = tokio::fs::remove_file(path).await;
}

/// Bind the daemon Unix socket.
fn bind_daemon_socket(path: &PathBuf) -> Result<UnixListener, ScribeError> {
    UnixListener::bind(path).map_err(|e| ScribeError::IpcError {
        reason: format!("failed to bind daemon socket at {}: {e}", path.display()),
    })
}

// ---------------------------------------------------------------------------
// Server message reader loop
// ---------------------------------------------------------------------------

/// Continuously read `ServerMessage`s and dispatch to session state.
async fn server_reader_loop(
    mut reader: tokio::net::unix::OwnedReadHalf,
    state: SharedState,
    notifiers: Arc<WaitNotifiers>,
) -> Result<(), ScribeError> {
    loop {
        let msg: ServerMessage = crate::ipc::recv(&mut reader).await?;
        dispatch_server_message(msg, &state, &notifiers).await;
    }
}

/// Dispatch a single `ServerMessage` to the appropriate session state.
#[allow(
    clippy::cognitive_complexity,
    reason = "flat match dispatch on server message variants; each arm is a one-line delegation"
)]
async fn dispatch_server_message(
    msg: ServerMessage,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    match msg {
        ServerMessage::PtyOutput { session_id, data } => {
            handle_pty_output(session_id, &data, state, notifiers).await;
        }
        ServerMessage::ScreenSnapshot { session_id, snapshot } => {
            handle_screen_snapshot(session_id, snapshot, state).await;
        }
        ServerMessage::CwdChanged { session_id, cwd } => {
            handle_cwd_changed(session_id, cwd, state, notifiers).await;
        }
        ServerMessage::TitleChanged { session_id, title } => {
            handle_title_changed(session_id, title, state).await;
        }
        ServerMessage::SessionCreated { session_id, workspace_id, shell_name } => {
            handle_session_created(session_id, workspace_id, &shell_name, state, notifiers).await;
        }
        ServerMessage::SessionExited { session_id, exit_code } => {
            handle_session_exited(session_id, exit_code, state, notifiers).await;
        }
        ServerMessage::WorkspaceInfo { workspace_id, .. } => {
            handle_workspace_info(workspace_id, state, notifiers).await;
        }
        ServerMessage::AiStateChanged { session_id, ai_state } => {
            debug!(%session_id, ?ai_state, "AI state changed");
        }
        ServerMessage::GitBranch { session_id, branch } => {
            debug!(%session_id, ?branch, "git branch updated");
        }
        ServerMessage::WorkspaceNamed { workspace_id, name } => {
            debug!(%workspace_id, %name, "workspace named");
        }
        ServerMessage::Bell { session_id } => {
            debug!(%session_id, "bell");
        }
        ServerMessage::Error { message } => {
            error!(%message, "server error");
        }
        ServerMessage::SessionList { .. } => {
            debug!("received session list (ignored by test daemon)");
        }
        ServerMessage::ScrolledSnapshot { session_id, .. } => {
            debug!(%session_id, "scrolled snapshot (ignored by test daemon)");
        }
        ServerMessage::SearchResults { session_id, .. } => {
            debug!(%session_id, "search results (ignored by test daemon)");
        }
        ServerMessage::Welcome { window_id, .. } => {
            debug!(%window_id, "welcome (ignored by test daemon)");
        }
        ServerMessage::QuitRequested => {
            debug!("quit requested (ignored by test daemon)");
        }
    }
}

// ---------------------------------------------------------------------------
// Individual server message handlers
// ---------------------------------------------------------------------------

async fn handle_pty_output(
    session_id: SessionId,
    data: &[u8],
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.output_buffer.extend(data);
        drain_output_buffer(session_id, &mut session.output_buffer);
        drop(guard);
        notifiers.output.notify_waiters();
    }
}

/// Trim the front of the output buffer if it exceeds the capacity limit.
///
/// Logs a warning when bytes are discarded so that `wait-output` failures
/// caused by buffer overflow are diagnosable.
fn drain_output_buffer(session_id: SessionId, buf: &mut VecDeque<u8>) {
    if buf.len() > MAX_OUTPUT_BUFFER {
        let excess = buf.len() - MAX_OUTPUT_BUFFER;
        warn!(
            %session_id,
            discarded_bytes = excess,
            "output buffer overflow — oldest bytes discarded; wait-output may miss matches"
        );
        buf.drain(..excess);
    }
}

async fn handle_screen_snapshot(
    session_id: SessionId,
    snapshot: ScreenSnapshot,
    state: &SharedState,
) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.latest_snapshot = Some(snapshot);
        session.snapshot_time = Some(tokio::time::Instant::now());
    }
}

async fn handle_cwd_changed(
    session_id: SessionId,
    cwd: PathBuf,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.cwd = Some(cwd);
        drop(guard);
        notifiers.cwd.notify_waiters();
    }
}

async fn handle_title_changed(session_id: SessionId, title: String, state: &SharedState) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.title = Some(title);
    }
}

async fn handle_session_created(
    session_id: SessionId,
    workspace_id: WorkspaceId,
    shell_name: &str,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    info!(%session_id, %workspace_id, %shell_name, "session created");
    let mut guard = state.lock().await;
    guard.sessions.insert(session_id, SessionState::new());
    guard.last_session_created = Some(session_id);
    drop(guard);
    notifiers.session_created.notify_waiters();
}

async fn handle_session_exited(
    session_id: SessionId,
    exit_code: Option<i32>,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    info!(%session_id, ?exit_code, "session exited");
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.status = SessionStatus::Exited(exit_code);
        drop(guard);
        notifiers.exit.notify_waiters();
    }
}

async fn handle_workspace_info(
    workspace_id: WorkspaceId,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    info!(%workspace_id, "workspace info received");
    let mut guard = state.lock().await;
    guard.last_workspace_id = Some(workspace_id);
    drop(guard);
    notifiers.workspace_info.notify_waiters();
}

// ---------------------------------------------------------------------------
// Command listener loop
// ---------------------------------------------------------------------------

/// Accept connections on the daemon socket and spawn a handler per client.
async fn command_listener_loop(
    listener: UnixListener,
    state: SharedState,
    notifiers: Arc<WaitNotifiers>,
    server_writer: Arc<Mutex<OwnedWriteHalf>>,
    shutdown: Arc<Notify>,
) {
    loop {
        let accepted = listener.accept().await;
        let stream = match accepted {
            Ok((stream, _addr)) => stream,
            Err(e) => {
                warn!("failed to accept daemon connection: {e}");
                continue;
            }
        };

        tokio::spawn(handle_client_connection(
            stream,
            Arc::clone(&state),
            Arc::clone(&notifiers),
            Arc::clone(&server_writer),
            Arc::clone(&shutdown),
        ));
    }
}

/// Handle a single client connection: read request, process, send response.
async fn handle_client_connection(
    stream: UnixStream,
    state: SharedState,
    notifiers: Arc<WaitNotifiers>,
    server_writer: Arc<Mutex<OwnedWriteHalf>>,
    shutdown: Arc<Notify>,
) {
    let (mut reader, mut writer) = stream.into_split();

    let request_result: Result<DaemonRequest, ScribeError> = read_message(&mut reader).await;

    let request = match request_result {
        Ok(r) => r,
        Err(e) => {
            warn!("failed to read daemon request: {e}");
            return;
        }
    };

    let response = process_request(request, &state, &notifiers, &server_writer, &shutdown).await;

    if let Err(e) = write_message(&mut writer, &response).await {
        warn!("failed to write daemon response: {e}");
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

/// Route a `DaemonRequest` to its handler.
async fn process_request(
    request: DaemonRequest,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
    shutdown: &Arc<Notify>,
) -> DaemonResponse {
    match request {
        DaemonRequest::CreateSession => {
            handle_create_session(state, notifiers, server_writer).await
        }
        DaemonRequest::AttachSession { session_id } => {
            handle_attach_session(session_id, state, notifiers, server_writer).await
        }
        DaemonRequest::CloseSession { session_id } => {
            handle_close_session(session_id, server_writer).await
        }
        DaemonRequest::Send { session_id, data } => {
            handle_send(session_id, data, server_writer).await
        }
        DaemonRequest::Resize { session_id, cols, rows } => {
            handle_resize(session_id, cols, rows, server_writer).await
        }
        DaemonRequest::RequestScreenshot { session_id }
        | DaemonRequest::RequestSnapshot { session_id } => {
            handle_request_snapshot(session_id, state, server_writer).await
        }
        DaemonRequest::WaitOutput { session_id, pattern, timeout_ms } => {
            handle_wait_output(session_id, &pattern, timeout_ms, state, notifiers).await
        }
        DaemonRequest::WaitCwd { session_id, path, timeout_ms } => {
            handle_wait_cwd(session_id, &path, timeout_ms, state, notifiers).await
        }
        DaemonRequest::WaitIdle { session_id, quiet_ms, timeout_ms } => {
            handle_wait_idle(session_id, quiet_ms, timeout_ms, notifiers).await
        }
        DaemonRequest::AssertCell { session_id, row, col, expected } => {
            let params = CellAssertParams { session_id, row, col, expected };
            handle_assert_cell(params, state, server_writer).await
        }
        DaemonRequest::AssertCursor { session_id, row, col } => {
            handle_assert_cursor(session_id, row, col, state, server_writer).await
        }
        DaemonRequest::AssertExit { session_id, expected_code, timeout_ms } => {
            handle_assert_exit(session_id, expected_code, timeout_ms, state, notifiers).await
        }
        DaemonRequest::AssertSnapshotMatch { session_id, reference } => {
            handle_assert_snapshot_match(session_id, &reference, state, server_writer).await
        }
        DaemonRequest::Shutdown => {
            handle_shutdown(shutdown);
            DaemonResponse::Ok
        }
    }
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

/// Create a workspace, then a session within it.
async fn handle_create_session(
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    // Step 1: Create a workspace.
    if let Err(e) = send_to_server(server_writer, &ClientMessage::CreateWorkspace).await {
        return DaemonResponse::Error { message: format!("failed to send CreateWorkspace: {e}") };
    }

    // Wait for WorkspaceInfo response.
    let Some(workspace_id) = wait_for_workspace_id(state, notifiers, Duration::from_secs(5)).await
    else {
        return DaemonResponse::Error { message: "timed out waiting for WorkspaceInfo".to_owned() };
    };

    // Step 2: Create a session in that workspace.
    let msg = ClientMessage::CreateSession { workspace_id, split_direction: None };
    if let Err(e) = send_to_server(server_writer, &msg).await {
        return DaemonResponse::Error { message: format!("failed to send CreateSession: {e}") };
    }

    // Wait for SessionCreated response.
    wait_for_session_created(state, notifiers, Duration::from_secs(5)).await.map_or_else(
        || DaemonResponse::Error { message: "timed out waiting for SessionCreated".to_owned() },
        |session_id| DaemonResponse::SessionCreated { session_id },
    )
}

/// Attach to an existing (detached) session on the server.
///
/// Sends `AttachSessions` + `Subscribe`, waits for the server to confirm
/// by sending `SessionCreated`, then registers the session in daemon state.
async fn handle_attach_session(
    session_id: SessionId,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let msg = ClientMessage::AttachSessions { session_ids: vec![session_id] };
    if let Err(e) = send_to_server(server_writer, &msg).await {
        return DaemonResponse::Error { message: format!("failed to send AttachSessions: {e}") };
    }

    // The server responds with SessionCreated for each attached session.
    let confirmed = wait_for_session_created(state, notifiers, Duration::from_secs(5)).await;

    // Also subscribe so the daemon gets CWD fallback checks.
    let sub = ClientMessage::Subscribe { session_ids: vec![session_id] };
    if let Err(e) = send_to_server(server_writer, &sub).await {
        warn!("failed to send Subscribe after attach: {e}");
    }

    confirmed.map_or_else(
        || DaemonResponse::Error { message: "timed out waiting for session attach".to_owned() },
        |sid| DaemonResponse::SessionCreated { session_id: sid },
    )
}

/// Wait for the daemon to receive a `WorkspaceInfo` message, returning the ID.
async fn wait_for_workspace_id(
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    timeout: Duration,
) -> Option<WorkspaceId> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let guard = state.lock().await;
            if let Some(id) = guard.last_workspace_id {
                return Some(id);
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let result = tokio::time::timeout(remaining, notifiers.workspace_info.notified()).await;
        if result.is_err() {
            return None;
        }
    }
}

/// Wait for the daemon to receive a `SessionCreated` message.
async fn wait_for_session_created(
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    timeout: Duration,
) -> Option<SessionId> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let guard = state.lock().await;
            if let Some(id) = guard.last_session_created {
                return Some(id);
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let result = tokio::time::timeout(remaining, notifiers.session_created.notified()).await;
        if result.is_err() {
            return None;
        }
    }
}

async fn handle_close_session(
    session_id: SessionId,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let msg = ClientMessage::CloseSession { session_id };
    match send_to_server(server_writer, &msg).await {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error { message: format!("failed to send CloseSession: {e}") },
    }
}

async fn handle_send(
    session_id: SessionId,
    data: Vec<u8>,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let msg = ClientMessage::KeyInput { session_id, data };
    match send_to_server(server_writer, &msg).await {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error { message: format!("failed to send KeyInput: {e}") },
    }
}

async fn handle_resize(
    session_id: SessionId,
    cols: u16,
    rows: u16,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let msg = ClientMessage::Resize { session_id, cols, rows };
    match send_to_server(server_writer, &msg).await {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error { message: format!("failed to send Resize: {e}") },
    }
}

/// Request a snapshot and wait for it to arrive.
async fn handle_request_snapshot(
    session_id: SessionId,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let msg = ClientMessage::RequestSnapshot { session_id };
    if let Err(e) = send_to_server(server_writer, &msg).await {
        return DaemonResponse::Error { message: format!("failed to send RequestSnapshot: {e}") };
    }

    // Poll for snapshot to arrive (up to 5 seconds).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(snap) = lookup_snapshot(session_id, state).await {
            return DaemonResponse::ScreenshotData { snapshot: Box::new(snap) };
        }
        if tokio::time::Instant::now() >= deadline {
            return DaemonResponse::Error { message: "timed out waiting for snapshot".to_owned() };
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Look up the latest snapshot for a session, if one exists.
async fn lookup_snapshot(session_id: SessionId, state: &SharedState) -> Option<ScreenSnapshot> {
    let guard = state.lock().await;
    guard.sessions.get(&session_id).and_then(|s| s.latest_snapshot.clone())
}

/// Wait for output matching a regex pattern.
async fn handle_wait_output(
    session_id: SessionId,
    pattern: &str,
    timeout_ms: u64,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let re = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return DaemonResponse::Error { message: format!("invalid regex: {e}") },
    };

    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if check_output_match(session_id, &re, state).await {
            return DaemonResponse::Ok;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return DaemonResponse::Error {
                message: format!("timed out waiting for output matching /{pattern}/"),
            };
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "timeout expiry is handled by the loop condition"
        )]
        let _ = tokio::time::timeout(remaining, notifiers.output.notified()).await;
    }
}

/// Check if the session's output buffer matches the given regex.
async fn check_output_match(session_id: SessionId, re: &Regex, state: &SharedState) -> bool {
    let guard = state.lock().await;
    let Some(session) = guard.sessions.get(&session_id) else {
        return false;
    };
    let buf: Vec<u8> = session.output_buffer.iter().copied().collect();
    let text = String::from_utf8_lossy(&buf);
    re.is_match(&text)
}

/// Wait until the session's CWD matches the given path.
async fn handle_wait_cwd(
    session_id: SessionId,
    path: &str,
    timeout_ms: u64,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let expected = PathBuf::from(path);
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if check_cwd_match(session_id, &expected, state).await {
            return DaemonResponse::Ok;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return DaemonResponse::Error {
                message: format!("timed out waiting for CWD to match {path}"),
            };
        }
        #[allow(
            clippy::let_underscore_must_use,
            reason = "timeout expiry is handled by the loop condition"
        )]
        let _ = tokio::time::timeout(remaining, notifiers.cwd.notified()).await;
    }
}

/// Check if the session's CWD matches the expected path.
async fn check_cwd_match(session_id: SessionId, expected: &PathBuf, state: &SharedState) -> bool {
    let guard = state.lock().await;
    let Some(session) = guard.sessions.get(&session_id) else {
        return false;
    };
    session.cwd.as_ref() == Some(expected)
}

/// Wait until no PTY output arrives for `quiet_ms` duration.
async fn handle_wait_idle(
    session_id: SessionId,
    quiet_ms: u64,
    timeout_ms: u64,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let quiet = Duration::from_millis(quiet_ms);
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return DaemonResponse::Error {
                message: format!("timed out waiting for {session_id} to be idle for {quiet_ms}ms"),
            };
        }

        // Wait for quiet period or new output.
        let wait_time = quiet.min(remaining);
        let result = tokio::time::timeout(wait_time, notifiers.output.notified()).await;
        if result.is_err() {
            // Timeout means no output for wait_time — idle achieved.
            return DaemonResponse::Ok;
        }
        // Output arrived — reset the quiet timer and loop again.
    }
}

/// Parameters for a cell assertion.
struct CellAssertParams {
    session_id: SessionId,
    row: u16,
    col: u16,
    expected: char,
}

/// Assert that a cell at (row, col) contains the expected character.
async fn handle_assert_cell(
    params: CellAssertParams,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let snap = get_or_request_snapshot(params.session_id, state, server_writer).await;
    let Some(snap) = snap else {
        return DaemonResponse::Error { message: "failed to obtain snapshot".to_owned() };
    };

    check_cell_content(&snap, params.row, params.col, params.expected)
}

/// Compare a single cell in the snapshot against the expected character.
/// On failure, includes a 3x3 neighborhood for context.
fn check_cell_content(snap: &ScreenSnapshot, row: u16, col: u16, expected: char) -> DaemonResponse {
    let cols = usize::from(snap.cols);
    let index = usize::from(row) * cols + usize::from(col);
    match snap.cells.get(index) {
        Some(cell) if cell.c == expected => DaemonResponse::Ok,
        Some(cell) => {
            let context = cell_neighborhood(snap, row, col);
            DaemonResponse::AssertFailed {
                message: format!(
                    "cell ({row},{col}): expected '{expected}' but found '{}'\n  context:\n{context}",
                    cell.c,
                ),
            }
        }
        None => {
            DaemonResponse::AssertFailed { message: format!("cell ({row},{col}): out of bounds") }
        }
    }
}

/// Replace control characters with a dot for display.
fn printable_char(c: char) -> char {
    if c.is_control() { '.' } else { c }
}

/// Build a 3-row context string around the target cell for debugging.
fn cell_neighborhood(snap: &ScreenSnapshot, row: u16, col: u16) -> String {
    let cols = usize::from(snap.cols);
    let rows = usize::from(snap.rows);
    let mut lines = Vec::new();

    let r_start = row.saturating_sub(1);
    let r_end = (usize::from(row) + 2).min(rows);
    let c_start = col.saturating_sub(3);
    let c_end = (usize::from(col) + 4).min(cols);

    for r in usize::from(r_start)..r_end {
        let mut line = format!("    row {r:3}: |");
        for c in usize::from(c_start)..c_end {
            let idx = r * cols + c;
            let ch = snap.cells.get(idx).map_or(' ', |cell| printable_char(cell.c));
            line.push(ch);
        }
        line.push('|');
        if r == usize::from(row) {
            line.push_str(" <--");
        }
        lines.push(line);
    }

    lines.join("\n")
}

/// Assert that the current screen matches a reference snapshot.
///
/// Compares non-space cell content, cursor position, and cursor visibility.
/// Reports the first mismatch found.
async fn handle_assert_snapshot_match(
    session_id: SessionId,
    reference: &ScreenSnapshot,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let snap = get_or_request_snapshot(session_id, state, server_writer).await;
    let Some(current) = snap else {
        return DaemonResponse::Error { message: "failed to obtain snapshot".to_owned() };
    };

    // Dimension mismatch.
    if current.cols != reference.cols || current.rows != reference.rows {
        return DaemonResponse::AssertFailed {
            message: format!(
                "snapshot size mismatch: current {}x{}, reference {}x{}",
                current.cols, current.rows, reference.cols, reference.rows,
            ),
        };
    }

    // Compare non-space cells (space cells are often padding and not meaningful).
    for (i, (cur, refr)) in current.cells.iter().zip(reference.cells.iter()).enumerate() {
        if refr.c != ' ' && cur.c != refr.c {
            let cols = usize::from(current.cols);
            let row = i / cols;
            let col = i % cols;
            return DaemonResponse::AssertFailed {
                message: format!("cell ({row},{col}): expected '{}' but found '{}'", refr.c, cur.c,),
            };
        }
    }

    // Compare cursor position.
    if current.cursor_row != reference.cursor_row || current.cursor_col != reference.cursor_col {
        return DaemonResponse::AssertFailed {
            message: format!(
                "cursor position mismatch: current ({},{}), reference ({},{})",
                current.cursor_row, current.cursor_col, reference.cursor_row, reference.cursor_col,
            ),
        };
    }

    // Compare cursor visibility.
    if current.cursor_visible != reference.cursor_visible {
        return DaemonResponse::AssertFailed {
            message: format!(
                "cursor visibility mismatch: current {}, reference {}",
                current.cursor_visible, reference.cursor_visible,
            ),
        };
    }

    DaemonResponse::Ok
}

/// Assert that the cursor is at the expected position.
async fn handle_assert_cursor(
    session_id: SessionId,
    row: u16,
    col: u16,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let snapshot = get_or_request_snapshot(session_id, state, server_writer).await;
    let Some(snap) = snapshot else {
        return DaemonResponse::Error { message: "failed to obtain snapshot".to_owned() };
    };

    if snap.cursor_row == row && snap.cursor_col == col {
        DaemonResponse::Ok
    } else {
        DaemonResponse::AssertFailed {
            message: format!(
                "cursor: expected ({row},{col}) but found ({},{})",
                snap.cursor_row, snap.cursor_col
            ),
        }
    }
}

/// Maximum age for a cached snapshot to be considered fresh. Assertions that
/// run in quick succession reuse the cached snapshot instead of round-tripping
/// to the server for each one.
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(100);

/// Get a recent snapshot, or request a fresh one from the server.
///
/// Returns the cached snapshot if it is less than [`SNAPSHOT_CACHE_TTL`] old,
/// avoiding redundant round-trips when multiple assertions run in sequence.
async fn get_or_request_snapshot(
    session_id: SessionId,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> Option<ScreenSnapshot> {
    // Return the cached snapshot if fresh enough.
    if let Some(snap) = lookup_fresh_snapshot(session_id, state).await {
        return Some(snap);
    }

    // Request one from the server.
    // Clear the stale snapshot first so the poll loop waits for the fresh one.
    {
        let mut guard = state.lock().await;
        if let Some(session) = guard.sessions.get_mut(&session_id) {
            session.latest_snapshot = None;
            session.snapshot_time = None;
        }
    }

    let msg = ClientMessage::RequestSnapshot { session_id };
    if send_to_server(server_writer, &msg).await.is_err() {
        return None;
    }

    // Poll for it to arrive.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Some(snap) = lookup_snapshot(session_id, state).await {
            return Some(snap);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
    }
}

/// Return the cached snapshot only if it was received within the cache TTL.
async fn lookup_fresh_snapshot(
    session_id: SessionId,
    state: &SharedState,
) -> Option<ScreenSnapshot> {
    let guard = state.lock().await;
    let session = guard.sessions.get(&session_id)?;
    let time = session.snapshot_time?;
    if time.elapsed() < SNAPSHOT_CACHE_TTL { session.latest_snapshot.clone() } else { None }
}

/// Assert that a session exited with the expected code.
async fn handle_assert_exit(
    session_id: SessionId,
    expected_code: i32,
    timeout_ms: u64,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let exit_status = check_exit_status(session_id, state).await;
        match exit_status {
            Some(SessionStatus::Exited(code)) => {
                return match code {
                    Some(c) if c == expected_code => DaemonResponse::Ok,
                    Some(c) => DaemonResponse::AssertFailed {
                        message: format!("exit code: expected {expected_code} but got {c}"),
                    },
                    None => DaemonResponse::AssertFailed {
                        message: format!(
                            "exit code: expected {expected_code} but session exited without code"
                        ),
                    },
                };
            }
            Some(SessionStatus::Running) | None => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return DaemonResponse::Error {
                        message: format!("timed out waiting for {session_id} to exit"),
                    };
                }
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "timeout expiry is handled by the loop condition"
                )]
                let _ = tokio::time::timeout(remaining, notifiers.exit.notified()).await;
            }
        }
    }
}

/// Check the current exit status of a session.
async fn check_exit_status(session_id: SessionId, state: &SharedState) -> Option<SessionStatus> {
    let guard = state.lock().await;
    guard.sessions.get(&session_id).map(|s| s.status.clone())
}

/// Signal the daemon to shut down.
fn handle_shutdown(shutdown: &Arc<Notify>) {
    info!("shutdown requested");
    shutdown.notify_one();
}

// ---------------------------------------------------------------------------
// Server send helper
// ---------------------------------------------------------------------------

/// Send a `ClientMessage` to the scribe-server via the shared writer.
async fn send_to_server(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    msg: &ClientMessage,
) -> Result<(), ScribeError> {
    let mut guard = writer.lock().await;
    crate::ipc::send(&mut guard, msg).await
}
