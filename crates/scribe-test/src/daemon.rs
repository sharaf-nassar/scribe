//! Persistent daemon that holds an IPC connection to scribe-server,
//! buffers per-session state, and serves subcommand requests over a local
//! Unix socket.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::{Regex, RegexBuilder};
use scribe_common::ai_state::AiProvider;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId, new_launch_id};
use scribe_common::protocol::{
    AiLaunchSpec, AiResumeMode, AutomationAction, ClientMessage, ServerMessage, TerminalSize,
};
use scribe_common::screen::ScreenSnapshot;
use scribe_common::screen_replay::SessionReplay;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::cmd_socket::{
    DaemonRequest, DaemonResponse, ReplayFrameInfo, daemon_socket_path, send_request,
};
use crate::replay::ReplayView;

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
    Exited(ChildExit),
}

/// The wait status a `SessionExited` frame reported, plus how many such frames
/// arrived (spec 017 US1-2).
///
/// The count is asserted, not just recorded: every exit path funnels through
/// one compare-and-swap on the server, so a second frame for the same session
/// is a real defect and the exit assertions fail on it.
#[derive(Debug, Clone, Copy, Default)]
struct ChildExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
    frames: u32,
}

/// Per-session state buffered by the daemon.
#[derive(Debug)]
struct SessionState {
    output_buffer: VecDeque<u8>,
    latest_snapshot: Option<ScreenSnapshot>,
    /// When the latest snapshot was received, for cache freshness checks.
    snapshot_time: Option<tokio::time::Instant>,
    /// When the most recent PTY output was received for this session.
    last_output_at: Option<tokio::time::Instant>,
    cwd: Option<PathBuf>,
    title: Option<String>,
    icon_title: Option<String>,
    status: SessionStatus,
    /// Latest AI context-window fill percentage, from `AiStateChanged`.
    ///
    /// Held independently of the AI state itself, mirroring the clients'
    /// decoupled context store: a state edge without a fresh `context=NN`
    /// keeps the previous percentage visible.
    ai_context: Option<u8>,
    /// Whether the session's current AI state pulses for attention
    /// (`PermissionPrompt` / `WaitingForInput`), which suppresses the tab
    /// context suffix so the pulse owns the UX.
    ai_pulsing: bool,
    /// Total `PtyOutput` bytes received for this session, never trimmed.
    ///
    /// The ring buffer above is capped, so its length cannot place a replay in
    /// the frame order; this counter can, and it is what
    /// [`ReplayFrameInfo::live_bytes_before`] is stamped from.
    live_bytes: u64,
    /// Zero-byte `PtyOutput` frames received for this session.
    ///
    /// Every one of them is a defect: a filtered-to-nothing chunk costs an
    /// allocation, a serialize, a queue slot, and a coalesce pass on every
    /// attached client while changing no cell. The count is what
    /// `assert-no-empty-output` reads.
    empty_output_frames: u64,
    /// What the session's replay path has delivered, and the screen rebuilt
    /// from it.
    replay: ReplayLog,
}

impl SessionState {
    fn new() -> Self {
        Self {
            output_buffer: VecDeque::with_capacity(MAX_OUTPUT_BUFFER),
            latest_snapshot: None,
            snapshot_time: None,
            last_output_at: None,
            cwd: None,
            title: None,
            icon_title: None,
            status: SessionStatus::Running,
            ai_context: None,
            ai_pulsing: false,
            live_bytes: 0,
            empty_output_frames: 0,
            replay: ReplayLog::default(),
        }
    }
}

/// Per-session `SessionReplay` bookkeeping plus the view rebuilt from it.
///
/// Replayed bytes deliberately stay out of `output_buffer`: keeping the ring
/// buffer the raw live PTY stream is what lets an assertion distinguish "this
/// text came back in the replay" from "this text arrived after the replay",
/// which is the whole question the buffered-flush ordering work has to answer.
#[derive(Debug, Default)]
struct ReplayLog {
    /// Frames inflated and applied, in arrival order.
    applied: u32,
    /// Frames that could not be inflated (zero dimensions or a corrupt blob).
    failed: u32,
    /// Metadata for the most recently applied frame.
    last: Option<ReplayFrameInfo>,
    /// Terminal holding the last replay plus every `PtyOutput` byte since.
    view: Option<ReplayView>,
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
    /// Window ID the server assigned this daemon in its `Welcome`. Surfaced by
    /// `scribe-test window-id` so a second local process — the visual rig's GPUI
    /// client — can join this window's share instead of claiming its own.
    window_id: Option<WindowId>,
    /// Launch (env-envelope) id minted for each session this daemon created,
    /// surfaced by `scribe-test session envelope-id` so an E2E script can assert
    /// that the harness create path really names an envelope.
    envelope_ids: HashMap<SessionId, String>,
    /// Most recent action the server asked this daemon window to run.
    last_action: Option<AutomationAction>,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            last_workspace_id: None,
            last_session_created: None,
            window_id: None,
            envelope_ids: HashMap::new(),
            last_action: None,
        }
    }
}

/// Notification channels used to wake up waiting request handlers.
struct WaitNotifiers {
    output: Arc<Notify>,
    cwd: Arc<Notify>,
    exit: Arc<Notify>,
    workspace_info: Arc<Notify>,
    session_created: Arc<Notify>,
    replay: Arc<Notify>,
}

impl WaitNotifiers {
    fn new() -> Self {
        Self {
            output: Arc::new(Notify::new()),
            cwd: Arc::new(Notify::new()),
            exit: Arc::new(Notify::new()),
            workspace_info: Arc::new(Notify::new()),
            session_created: Arc::new(Notify::new()),
            replay: Arc::new(Notify::new()),
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

/// Print the most recent automation action, or `none` when the oracle is empty.
pub fn print_last_action() -> Result<(), ScribeError> {
    let response = send_request(&DaemonRequest::LastAction)?;
    match response {
        DaemonResponse::LastAction { action } => {
            let value = action.map_or_else(|| "none".to_owned(), |action| format!("{action:?}"));
            writeln!(io::stdout(), "{value}")?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(ScribeError::IpcError { reason: message }),
        other => Err(ScribeError::ProtocolError {
            reason: format!("unexpected daemon response: {other:?}"),
        }),
    }
}

/// Reset the automation-action oracle to its deterministic empty state.
pub fn clear_last_action() -> Result<(), ScribeError> {
    match send_request(&DaemonRequest::ClearAction)? {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(ScribeError::IpcError { reason: message }),
        other => Err(ScribeError::ProtocolError {
            reason: format!("unexpected daemon response: {other:?}"),
        }),
    }
}

/// Run the daemon event loop (foreground). This is the `daemon run` entry.
pub async fn run() -> Result<(), ScribeError> {
    let state: SharedState = Arc::new(Mutex::new(DaemonState::new()));
    let notifiers = Arc::new(WaitNotifiers::new());

    let server_conn = crate::ipc::connect().await?;
    let (server_reader, mut raw_server_writer) = server_conn.into_split();

    // Send `Hello { window_id: None }` so the server's `resolve_window_assignment`
    // can adopt an unconnected window-with-sessions. Without this, every fresh
    // daemon process gets a brand-new `WindowId` via `handle_legacy_client`, and
    // any `AttachSessions` for sessions whose owning window is the previous
    // daemon's `WindowId` is denied. That breaks the reconnect flow:
    // daemon stop → daemon start → session attach.
    crate::ipc::send(
        &mut raw_server_writer,
        &ClientMessage::Hello {
            window_id: None,
            clipboard_gating: false,
            takeover: false,
            // Read the same default-on `terminal.images.enabled` switch the
            // client reads, so the harness daemon stands in for a real viewer
            // instead of mirroring it by hand. A script that wants a text-only
            // live server writes `enabled = false` into the container's config
            // — exactly the rollback a user would perform.
            // @lat: [[test#Terminal Image Safety and Continuity#Live Capable Viewer]]
            terminal_images: scribe_common::terminal_images::advertised_capabilities(),
        },
    )
    .await?;

    let server_writer = Arc::new(Mutex::new(raw_server_writer));

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
    drop(tokio::fs::remove_file(path).await);
}

/// Remove the daemon socket on shutdown.
async fn cleanup_socket(path: &PathBuf) {
    drop(tokio::fs::remove_file(path).await);
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
async fn dispatch_server_message(
    msg: ServerMessage,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    match msg {
        msg @ (ServerMessage::PtyOutput { .. }
        | ServerMessage::ScreenSnapshot { .. }
        | ServerMessage::SessionReplay { .. }
        | ServerMessage::CwdChanged { .. }
        | ServerMessage::TitleChanged { .. }
        | ServerMessage::IconTitleChanged { .. }
        | ServerMessage::SessionCreated { .. }
        | ServerMessage::SessionExited { .. }
        | ServerMessage::SessionContextChanged { .. }
        | ServerMessage::TrimScrollback { .. }
        | ServerMessage::ScrollBottom { .. }
        | ServerMessage::AiStateChanged { .. }
        | ServerMessage::AiStateCleared { .. }) => {
            dispatch_session_message(msg, state, notifiers).await;
        }
        msg @ (ServerMessage::WorkspaceInfo { .. }
        | ServerMessage::WorkspaceNamed { .. }
        | ServerMessage::Welcome { .. }
        | ServerMessage::WindowClosed { .. }
        | ServerMessage::QuitRequested
        | ServerMessage::UpdateAvailable { .. }
        | ServerMessage::UpdateProgress { .. }
        | ServerMessage::UpdateCheckResult { .. }
        | ServerMessage::ReleaseList { .. }
        | ServerMessage::WindowList { .. }
        | ServerMessage::RunAction { .. }
        | ServerMessage::ActionDispatched { .. }) => {
            dispatch_window_message(msg, state, notifiers).await;
        }
        msg @ (ServerMessage::CodexTaskLabelChanged { .. }
        | ServerMessage::CodexTaskLabelCleared { .. }
        | ServerMessage::TaskLabelChanged { .. }
        | ServerMessage::TaskLabelCleared { .. }
        | ServerMessage::GitBranch { .. }
        | ServerMessage::Bell { .. }
        | ServerMessage::Error { .. }
        | ServerMessage::SessionList { .. }
        | ServerMessage::SearchResults { .. }
        | ServerMessage::PromptMark { .. }
        | ServerMessage::PromptReceived { .. }) => {
            dispatch_notice_message(msg);
        }
        // Test daemon does not exercise env-persistence (feature 006), OSC 52
        // clipboard gating (spec 010), remote window control (feature 013), or
        // LAN remote control (feature 014) flows yet. The feature 014 LAN
        // messages below are consumed by the client/settings surfaces (tasks
        // T014/T018/T019/T020/T024); this is a behavior-preserving no-op arm.
        ServerMessage::EnvPreflightResult { .. }
        | ServerMessage::EnvStatus { .. }
        | ServerMessage::ClipboardPromptRequest { .. }
        | ServerMessage::ClipboardBridgeWrite { .. }
        | ServerMessage::ClipboardBridgeReadRequest { .. }
        | ServerMessage::RemoteHandshakeReply { .. }
        | ServerMessage::WindowTakenOver { .. }
        | ServerMessage::RemoteDisconnect { .. }
        | ServerMessage::RemotePeerList { .. }
        | ServerMessage::RemoteEnv { .. }
        | ServerMessage::LanApprovalPending
        | ServerMessage::LanApprovalResult { .. }
        | ServerMessage::LanApprovalRequest { .. }
        | ServerMessage::LanPeerList { .. }
        | ServerMessage::TrustedDeviceList { .. }
        | ServerMessage::TrustedNetworkList { .. }
        | ServerMessage::LanEnv { .. }
        | ServerMessage::LanDialIdentity { .. }
        | ServerMessage::TerminalImageLive { .. }
        | ServerMessage::TerminalImageReplay { .. }
        | ServerMessage::TerminalImageCapabilityMismatch { .. }
        | ServerMessage::BeadsBoard { .. }
        | ServerMessage::ShareRoster { .. }
        | ServerMessage::ControlRequested { .. }
        | ServerMessage::ControlDenied { .. }
        | ServerMessage::ShareEnded { .. } => {}
    }
}

async fn dispatch_session_message(
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
        ServerMessage::SessionReplay { session_id, replay } => {
            handle_session_replay(session_id, &replay, state, notifiers).await;
        }
        ServerMessage::CwdChanged { session_id, cwd } => {
            handle_cwd_changed(session_id, cwd, state, notifiers).await;
        }
        ServerMessage::TitleChanged { session_id, title } => {
            handle_title_changed(session_id, title, state).await;
        }
        ServerMessage::IconTitleChanged { session_id, title } => {
            handle_icon_title_changed(session_id, title, state).await;
        }
        ServerMessage::SessionCreated { session_id, workspace_id, shell_name } => {
            handle_session_created(session_id, workspace_id, &shell_name, state, notifiers).await;
        }
        ServerMessage::SessionExited { session_id, exit_code, signal } => {
            handle_session_exited(session_id, exit_code, signal, state, notifiers).await;
        }
        ServerMessage::SessionContextChanged { session_id, context } => {
            debug!(%session_id, ?context, "session context changed (ignored by test daemon)");
        }
        ServerMessage::TrimScrollback { session_id, history_rows } => {
            debug!(%session_id, history_rows, "trim scrollback (ignored by test daemon)");
        }
        ServerMessage::ScrollBottom { session_id } => {
            debug!(%session_id, "scroll bottom (ignored by test daemon)");
        }
        ServerMessage::AiStateChanged { session_id, ai_state } => {
            handle_ai_state_changed(session_id, &ai_state, state).await;
        }
        ServerMessage::AiStateCleared { session_id } => {
            handle_ai_state_cleared(session_id, state).await;
        }
        other => debug!(?other, "ignored non-session server message in session dispatcher"),
    }
}

async fn dispatch_window_message(
    msg: ServerMessage,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    match msg {
        ServerMessage::WorkspaceInfo { workspace_id, .. } => {
            handle_workspace_info(workspace_id, state, notifiers).await;
        }
        ServerMessage::WorkspaceNamed { workspace_id, name, .. } => {
            debug!(%workspace_id, %name, "workspace named");
        }
        ServerMessage::Welcome { window_id, .. } => {
            debug!(%window_id, "welcome; recording the daemon's window id");
            state.lock().await.window_id = Some(window_id);
        }
        ServerMessage::WindowClosed { window_id } => {
            debug!(%window_id, "window closed (ignored by test daemon)");
        }
        ServerMessage::QuitRequested => {
            debug!("quit requested (ignored by test daemon)");
        }
        ServerMessage::UpdateAvailable { version, .. } => {
            debug!(%version, "update available (ignored by test daemon)");
        }
        ServerMessage::UpdateProgress { .. } => {
            debug!("update progress (ignored by test daemon)");
        }
        ServerMessage::UpdateCheckResult { .. } => {
            debug!("update check result (ignored by test daemon)");
        }
        ServerMessage::WindowList { windows } => {
            debug!(count = windows.len(), "window list (ignored by test daemon)");
        }
        ServerMessage::RunAction { action } => {
            debug!(?action, "recording automation action");
            state.lock().await.last_action = Some(action);
        }
        ServerMessage::ActionDispatched { window_id } => {
            debug!(%window_id, "action dispatched (ignored by test daemon)");
        }
        other => debug!(?other, "ignored non-window server message in window dispatcher"),
    }
}

fn dispatch_notice_message(msg: ServerMessage) {
    if dispatch_task_label_notice(&msg) {
        return;
    }

    match msg {
        ServerMessage::GitBranch { session_id, branch } => {
            debug!(%session_id, ?branch, "git branch updated");
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
        ServerMessage::SearchResults { session_id, .. } => {
            debug!(%session_id, "search results (ignored by test daemon)");
        }
        ServerMessage::PromptMark { session_id, .. } => {
            debug!(%session_id, "prompt mark (ignored by test daemon)");
        }
        ServerMessage::PromptReceived { session_id, .. } => {
            debug!(%session_id, "prompt received (ignored by test daemon)");
        }
        other => debug!(?other, "ignored non-notice server message in notice dispatcher"),
    }
}

fn dispatch_task_label_notice(msg: &ServerMessage) -> bool {
    match msg {
        ServerMessage::CodexTaskLabelChanged { session_id, task_label } => {
            debug!(%session_id, %task_label, "codex task label changed (ignored by test daemon)");
        }
        ServerMessage::CodexTaskLabelCleared { session_id } => {
            debug!(%session_id, "codex task label cleared (ignored by test daemon)");
        }
        ServerMessage::TaskLabelChanged { session_id, provider, task_label } => {
            debug!(%session_id, ?provider, %task_label, "AI task label changed");
        }
        ServerMessage::TaskLabelCleared { session_id, provider } => {
            debug!(%session_id, ?provider, "AI task label cleared");
        }
        _ => return false,
    }
    true
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
        if data.is_empty() {
            // Recorded rather than ignored: the server drops filtered-to-nothing
            // chunks before framing them, so an empty frame here is a
            // regression `assert-no-empty-output` has to be able to name.
            session.empty_output_frames = session.empty_output_frames.saturating_add(1);
            warn!(%session_id, "received an empty PtyOutput frame");
        }
        session.output_buffer.extend(data);
        drain_output_buffer(session_id, &mut session.output_buffer);
        session.last_output_at = Some(tokio::time::Instant::now());
        session.live_bytes = session.live_bytes.saturating_add(data.len() as u64);
        // Keep the replayed view current so it stays comparable to the server's
        // screen. Sessions that never received a replay have no view and pay
        // nothing for this.
        if let Some(view) = session.replay.view.as_mut() {
            view.feed(data);
        }
        drop(guard);
        notifiers.output.notify_waiters();
    }
}

/// Inflate a `SessionReplay`, apply it to the session's local terminal, and
/// record where it landed in the session's frame order.
///
/// A frame that fails to inflate is counted and logged rather than fatal: a
/// client degrades to an error banner instead of tearing down its reader, and
/// the harness has to be able to assert on that same outcome.
async fn handle_session_replay(
    session_id: SessionId,
    replay: &SessionReplay,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    let mut guard = state.lock().await;
    let Some(session) = guard.sessions.get_mut(&session_id) else {
        warn!(%session_id, "session replay for a session the daemon does not track");
        return;
    };

    let live_bytes_before = session.live_bytes;
    match ReplayView::apply(session_id, replay) {
        Ok((view, inflated_bytes)) => {
            let index = session.replay.applied.saturating_add(1);
            session.replay.applied = index;
            session.replay.view = Some(view);
            session.replay.last = Some(ReplayFrameInfo {
                index,
                cols: replay.cols,
                rows: replay.rows,
                scrollback_rows: replay.scrollback_rows,
                cursor_row: replay.cursor_row,
                cursor_col: replay.cursor_col,
                alt_screen: replay.alt_screen,
                compressed_bytes: replay.replay_zstd.len(),
                inflated_bytes,
                live_bytes_before,
            });
            info!(
                %session_id,
                index,
                cols = replay.cols,
                rows = replay.rows,
                inflated_bytes,
                live_bytes_before,
                "session replay applied"
            );
        }
        Err(reason) => {
            session.replay.failed = session.replay.failed.saturating_add(1);
            warn!(%session_id, %reason, "session replay could not be applied");
        }
    }

    drop(guard);
    notifiers.replay.notify_waiters();
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

/// Record the AI context percentage and pulse state a client would render for
/// this session.
///
/// A `context` of `None` is *not* written back: the server only merges a fresh
/// `context=NN` when a producer emits one, and the clients keep showing the last
/// known percentage across state-only edges. Mirroring that here keeps
/// `RequestAiChrome` honest about what the chrome displays.
async fn handle_ai_state_changed(
    session_id: SessionId,
    ai_state: &scribe_common::ai_state::AiProcessState,
    state: &SharedState,
) {
    use scribe_common::ai_state::AiState;

    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        if let Some(context) = ai_state.context {
            session.ai_context = Some(context);
        }
        session.ai_pulsing =
            matches!(ai_state.state, AiState::PermissionPrompt | AiState::WaitingForInput);
        debug!(%session_id, ?ai_state.state, context = ?session.ai_context, "AI state changed");
    }
}

/// Drop a session's AI chrome so a cleared indicator stops reporting a stale
/// percentage.
async fn handle_ai_state_cleared(session_id: SessionId, state: &SharedState) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.ai_context = None;
        session.ai_pulsing = false;
    }
    debug!(%session_id, "AI state cleared");
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

async fn handle_icon_title_changed(session_id: SessionId, title: String, state: &SharedState) {
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        session.icon_title = Some(title);
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
    signal: Option<i32>,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) {
    info!(%session_id, ?exit_code, ?signal, "session exited");
    let mut guard = state.lock().await;
    if let Some(session) = guard.sessions.get_mut(&session_id) {
        let frames = match session.status {
            SessionStatus::Exited(previous) => previous.frames,
            SessionStatus::Running => 0,
        };
        session.status = SessionStatus::Exited(ChildExit { exit_code, signal, frames: frames + 1 });
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
        request @ DaemonRequest::CreateSession { .. } => match create_session_params(request) {
            Ok(params) => handle_create_session(params, state, notifiers, server_writer).await,
            Err(message) => DaemonResponse::Error { message },
        },
        DaemonRequest::AttachSession { session_id, cols, rows } => {
            let params = AttachParams { session_id, size: harness_size(cols, rows) };
            handle_attach_session(params, state, notifiers, server_writer).await
        }
        DaemonRequest::CloseSession { session_id } => {
            handle_close_session(session_id, server_writer).await
        }
        DaemonRequest::Send { session_id, data } => {
            handle_send(session_id, data, server_writer).await
        }
        DaemonRequest::Resize { session_id, cols, rows } => {
            handle_resize(session_id, cols, rows, state, server_writer).await
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
            handle_wait_idle(session_id, quiet_ms, timeout_ms, notifiers, state).await
        }
        DaemonRequest::AssertCell { session_id, row, col, expected } => {
            let params = CellAssertParams { session_id, row, col, expected };
            handle_assert_cell(params, state, server_writer).await
        }
        DaemonRequest::AssertCursor { session_id, row, col } => {
            handle_assert_cursor(session_id, row, col, state, server_writer).await
        }
        DaemonRequest::AssertExit { session_id, expected_code, timeout_ms } => {
            let expected = ExpectedExit::Code(expected_code);
            handle_assert_exit(session_id, expected, timeout_ms, state, notifiers).await
        }
        DaemonRequest::AssertSignal { session_id, expected_signal, timeout_ms } => {
            let expected = ExpectedExit::Signal(expected_signal);
            handle_assert_exit(session_id, expected, timeout_ms, state, notifiers).await
        }
        DaemonRequest::AssertNoEmptyOutput { session_id } => {
            handle_assert_no_empty_output(session_id, state).await
        }
        DaemonRequest::AssertSnapshotMatch { session_id, reference } => {
            handle_assert_snapshot_match(session_id, &reference, state, server_writer).await
        }
        DaemonRequest::RequestAiChrome { session_id } => {
            handle_request_ai_chrome(session_id, state).await
        }
        DaemonRequest::ReplayStatus { session_id, min_frames, timeout_ms } => {
            handle_replay_status(session_id, min_frames, timeout_ms, state, notifiers).await
        }
        DaemonRequest::ReplayScreen { session_id } => handle_replay_screen(session_id, state).await,
        DaemonRequest::AssertReplayMatchesScreen { session_id } => {
            handle_assert_replay_matches(session_id, state, server_writer).await
        }
        DaemonRequest::WindowId => handle_window_id(state).await,
        DaemonRequest::EnvelopeId { session_id } => handle_envelope_id(session_id, state).await,
        DaemonRequest::LastAction => {
            let action = state.lock().await.last_action.clone();
            DaemonResponse::LastAction { action }
        }
        DaemonRequest::ClearAction => {
            state.lock().await.last_action = None;
            DaemonResponse::Ok
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

/// Build the `TerminalSize` a `--cols` / `--rows` pair names, or `None` when the
/// caller named neither.
///
/// The cell box is 1x1 for the same reason [`handle_resize`] uses it: the
/// harness has no font, and matching that convention is what makes a create and
/// a later attach or resize at the same grid produce a byte-identical
/// `TIOCSWINSZ` — which the kernel answers with no `SIGWINCH` at all, so a test
/// can count the signals a geometry change really costs.
fn harness_size(cols: Option<u16>, rows: Option<u16>) -> Option<TerminalSize> {
    match (cols, rows) {
        (Some(cols), Some(rows)) => {
            Some(TerminalSize { cols, rows, cell_width: 1, cell_height: 1 })
        }
        _ => None,
    }
}

/// Everything one `AttachSession` request names. Grouped so the handler stays
/// under Clippy's argument threshold.
struct AttachParams {
    session_id: SessionId,
    size: Option<TerminalSize>,
}

/// Everything one harness session-create request contributes to the server
/// launch. Grouped so the async handler stays within Clippy's argument limit.
struct CreateSessionParams {
    size: Option<TerminalSize>,
    ai_provider: Option<AiProvider>,
    ai_resume_mode: Option<AiResumeMode>,
    ai_conversation_id: Option<String>,
    cwd: Option<PathBuf>,
    env_envelope_id: Option<String>,
}

fn create_session_params(request: DaemonRequest) -> Result<CreateSessionParams, String> {
    match request {
        DaemonRequest::CreateSession {
            cols,
            rows,
            ai_provider,
            ai_resume_mode,
            ai_conversation_id,
            cwd,
            env_envelope_id,
        } => Ok(CreateSessionParams {
            size: harness_size(cols, rows),
            ai_provider,
            ai_resume_mode,
            ai_conversation_id,
            cwd,
            env_envelope_id,
        }),
        _ => Err("internal request routing error: expected CreateSession".to_owned()),
    }
}

/// Create a workspace, then a session within it.
async fn handle_create_session(
    params: CreateSessionParams,
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
    // Clear stale session ID so wait_for_session_created doesn't return
    // the previous session's ID immediately.
    {
        let mut guard = state.lock().await;
        guard.last_session_created = None;
    }
    // The harness mints a launch id like every other create path unless a test
    // names an existing envelope to restore. Without either, the server has no
    // envelope to key this session's environment by.
    let envelope_id = params.env_envelope_id.unwrap_or_else(new_launch_id);
    let ai_launch = params.ai_provider.map(|provider| AiLaunchSpec {
        provider,
        resume_mode: params.ai_resume_mode.unwrap_or(AiResumeMode::New),
        conversation_id: params.ai_conversation_id,
    });
    let msg = ClientMessage::CreateSession {
        workspace_id,
        split_direction: None,
        cwd: params.cwd,
        size: params.size,
        command: None,
        ai_launch,
        env_envelope_id: Some(envelope_id.clone()),
    };
    if let Err(e) = send_to_server(server_writer, &msg).await {
        return DaemonResponse::Error { message: format!("failed to send CreateSession: {e}") };
    }

    // Wait for SessionCreated response.
    let Some(session_id) = wait_for_session_created(state, notifiers, Duration::from_secs(5)).await
    else {
        return DaemonResponse::Error {
            message: "timed out waiting for SessionCreated".to_owned(),
        };
    };
    // A create IS an attach — the server installed this connection's sink while
    // starting the session — so the harness follows it with the same `Subscribe`
    // a real client sends and deliberately with no `AttachSessions`: re-attaching
    // would replay a terminal that has emitted nothing and drive the PTY off the
    // grid this request just spawned it at.
    let sub = ClientMessage::Subscribe { session_ids: vec![session_id] };
    if let Err(e) = send_to_server(server_writer, &sub).await {
        warn!("failed to send Subscribe after create: {e}");
    }
    state.lock().await.envelope_ids.insert(session_id, envelope_id);
    DaemonResponse::SessionCreated { session_id }
}

/// Attach to an existing (detached) session on the server.
///
/// Sends `AttachSessions` + `Subscribe`, waits for the server to confirm
/// by sending `SessionCreated`, then registers the session in daemon state.
async fn handle_attach_session(
    params: AttachParams,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    let AttachParams { session_id, size } = params;
    let msg = ClientMessage::AttachSessions {
        session_ids: vec![session_id],
        dimensions: size.into_iter().collect(),
    };
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
    let msg = ClientMessage::KeyInput { session_id, data, dismisses_attention: true };
    match send_to_server(server_writer, &msg).await {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error { message: format!("failed to send KeyInput: {e}") },
    }
}

async fn handle_resize(
    session_id: SessionId,
    cols: u16,
    rows: u16,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    // Resize the replayed view alongside the real session; a view left at the
    // pre-resize geometry reports mismatches that belong to the harness rather
    // than to the server.
    {
        let mut guard = state.lock().await;
        if let Some(view) = guard.sessions.get_mut(&session_id).and_then(|s| s.replay.view.as_mut())
        {
            view.resize(cols, rows);
        }
    }

    let msg = ClientMessage::Resize {
        session_id,
        size: TerminalSize { cols, rows, cell_width: 1, cell_height: 1 },
    };
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

/// Render the AI chrome text for a session from its last `AiStateChanged`.
///
/// The strings come from [`scribe_common::ai_chrome`], the same module the
/// clients' prompt bar and tab bar format through, so an assertion here is an
/// assertion about what the chrome spells — not a second implementation of it.
/// Warn/danger bands come from the loaded config, matching how a client resolves
/// them.
async fn handle_request_ai_chrome(session_id: SessionId, state: &SharedState) -> DaemonResponse {
    let (context, pulsing) = {
        let guard = state.lock().await;
        let Some(session) = guard.sessions.get(&session_id) else {
            return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
        };
        (session.ai_context, session.ai_pulsing)
    };
    let Some(percent) = context else {
        return DaemonResponse::AiChrome { prompt_bar: None, tab: None };
    };
    let thresholds = scribe_common::config::load_config()
        .unwrap_or_default()
        .terminal
        .ai_session
        .context_thresholds;
    DaemonResponse::AiChrome {
        prompt_bar: Some(scribe_common::ai_chrome::context_meter_label(percent)),
        tab: scribe_common::ai_chrome::tab_context_suffix_text(percent, thresholds.warn, pulsing),
    }
}

/// Report a session's replay bookkeeping, optionally waiting for frames.
///
/// The attach reply is applied on the reader task, so a caller that asked for
/// an attach and immediately asked for status would race it; `min_frames`
/// blocks on the replay notifier until the frames land or the timeout expires.
/// A timeout is not an error here — the response still carries the observed
/// counts, and the CLI decides whether "fewer frames than asked for" is a
/// failure or the very thing the test asserts.
async fn handle_replay_status(
    session_id: SessionId,
    min_frames: u32,
    timeout_ms: u64,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        {
            let guard = state.lock().await;
            let Some(session) = guard.sessions.get(&session_id) else {
                return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
            };
            let log = &session.replay;
            if log.applied >= min_frames {
                return DaemonResponse::ReplayStatus {
                    applied: log.applied,
                    failed: log.failed,
                    live_bytes: session.live_bytes,
                    last: log.last.clone(),
                };
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let guard = state.lock().await;
            let Some(session) = guard.sessions.get(&session_id) else {
                return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
            };
            return DaemonResponse::ReplayStatus {
                applied: session.replay.applied,
                failed: session.replay.failed,
                live_bytes: session.live_bytes,
                last: session.replay.last.clone(),
            };
        }
        drop(tokio::time::timeout(remaining, notifiers.replay.notified()).await);
    }
}

/// Return the screen rebuilt from the session's replay plus the live output
/// that followed it.
async fn handle_replay_screen(session_id: SessionId, state: &SharedState) -> DaemonResponse {
    let guard = state.lock().await;
    let Some(session) = guard.sessions.get(&session_id) else {
        return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
    };
    session.replay.view.as_ref().map_or_else(
        || DaemonResponse::Error {
            message: format!("no replay has been applied for {session_id}"),
        },
        |view| DaemonResponse::ScreenshotData { snapshot: Box::new(view.snapshot()) },
    )
}

/// Compare the replayed view against the server's own screen.
///
/// The server's snapshot is requested fresh and read back together with the
/// view under one lock, so the two describe the same point in the session's
/// frame order: the reader applies output and the snapshot in arrival order, so
/// a difference means the wire disagreed with the server's `Term` — the attach
/// gap this assertion exists to catch. Callers still settle the session
/// (`wait-idle`) first, since output that arrives while the request is in
/// flight legitimately moves the view ahead.
async fn handle_assert_replay_matches(
    session_id: SessionId,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> DaemonResponse {
    {
        let guard = state.lock().await;
        match guard.sessions.get(&session_id) {
            None => {
                return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
            }
            Some(session) if session.replay.view.is_none() => {
                return DaemonResponse::Error {
                    message: format!("no replay has been applied for {session_id}"),
                };
            }
            Some(_) => {}
        }
    }

    let Some((server_screen, replayed)) =
        request_screen_pair(session_id, state, server_writer).await
    else {
        return DaemonResponse::Error { message: "failed to obtain snapshot".to_owned() };
    };

    // The server screen is the reference: everything it shows must be on the
    // replayed view. Cursor *visibility* is left out on purpose — the replay
    // encoder deliberately leaves the cursor hidden for alt-screen snapshots so
    // the app's own output owns it, so visibility is not a property a replay
    // promises to reproduce.
    compare_screen_content(&replayed, &server_screen)
        .or_else(|| compare_scrollback(&replayed, &server_screen))
        .map_or(DaemonResponse::Ok, |message| DaemonResponse::AssertFailed { message })
}

/// Compare the replayed view's scrollback against the server's.
///
/// The visible grid on its own cannot see output lost or duplicated during an
/// attach: a scrolling stream anchors the visible rows to the newest lines, so
/// both sides show the same tail no matter how many rows went missing behind
/// it. History is where the difference lands and stays, which makes the row
/// COUNT the sharp oracle — one lost chunk is one missing row forever, one
/// duplicated flush is one extra row forever. The content comparison behind it
/// catches the same defect once both sides sit at the scrollback cap, where the
/// counts are equal but the histories are offset.
fn compare_scrollback(current: &ScreenSnapshot, reference: &ScreenSnapshot) -> Option<String> {
    if current.scrollback_rows != reference.scrollback_rows {
        return Some(format!(
            "scrollback row mismatch: replayed view has {} rows, server has {} — \
             output was lost or duplicated between the attach snapshot and the sink install",
            current.scrollback_rows, reference.scrollback_rows,
        ));
    }

    let cols = usize::from(current.cols).max(1);
    for (i, (cur, refr)) in current.scrollback.iter().zip(reference.scrollback.iter()).enumerate() {
        if refr.c != ' ' && cur.c != refr.c {
            let row = i / cols;
            let col = i % cols;
            return Some(format!(
                "scrollback cell ({row},{col}): expected '{}' but found '{}'",
                refr.c, cur.c
            ));
        }
    }

    None
}

/// Request a fresh server snapshot and read it back paired with the replayed
/// view captured under the same lock.
async fn request_screen_pair(
    session_id: SessionId,
    state: &SharedState,
    server_writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> Option<(ScreenSnapshot, ScreenSnapshot)> {
    {
        let mut guard = state.lock().await;
        let session = guard.sessions.get_mut(&session_id)?;
        session.latest_snapshot = None;
        session.snapshot_time = None;
    }

    let msg = ClientMessage::RequestSnapshot { session_id };
    send_to_server(server_writer, &msg).await.ok()?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        {
            let guard = state.lock().await;
            let session = guard.sessions.get(&session_id)?;
            if let (Some(server_screen), Some(view)) =
                (session.latest_snapshot.clone(), session.replay.view.as_ref())
            {
                return Some((server_screen, view.snapshot()));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
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
    // Enable multi-line mode so `^` and `$` match line boundaries within the
    // session's accumulated output buffer (which is multi-line). Without this,
    // patterns like `^1$` would only match if the *entire* buffer were `1\n`,
    // which never happens once a shell prompt is present.
    let re = match RegexBuilder::new(pattern).multi_line(true).build() {
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
        drop(tokio::time::timeout(remaining, notifiers.output.notified()).await);
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
    // PTYs emit `\r\n` line endings. The `regex` crate's multi-line mode
    // anchors at `\n` boundaries, leaving the trailing `\r` inside each line
    // — so `^X$` patterns from test scripts would never match a line whose
    // raw content is `X\r`. Normalise to `\n`-only lines before matching.
    // Lone `\r` (rare cursor-return without newline) is preserved.
    // Strip ANSI/OSC/CSI escape sequences and lone CRs so `wait-output` patterns
    // match the *visible* terminal content rather than the raw PTY byte stream.
    // PTYs interleave OSC marks (shell integration), CSI sequences (modes,
    // colours), and cursor-return CRs with the actual characters; without this
    // normalisation `^X$`-style patterns can never match because each line's
    // raw content carries escape bytes around the visible text.
    let visible = strip_ansi_and_cr(&text);
    re.is_match(&visible)
}

/// Strip ANSI/OSC/CSI/DCS escape sequences and lone CRs so regex matching
/// operates on the visible content of the PTY stream. The pattern covers OSC
/// terminated by BEL or ST, DCS terminated by ST, CSI with standard parameter/
/// intermediate/final bytes, and other single-final ESC sequences.
fn strip_ansi_and_cr(s: &str) -> String {
    static ANSI: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    let built = ANSI.get_or_init(|| {
        RegexBuilder::new(
            r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1bP[^\x1b]*\x1b\\|\x1b\[[\x30-\x3f]*[\x20-\x2f]*[\x40-\x7e]|\x1b[\x20-\x5f]",
        )
        .build()
    });
    built
        .as_ref()
        .map_or_else(|_| s.to_owned(), |re| re.replace_all(s, "").into_owned())
        .replace('\r', "")
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
        drop(tokio::time::timeout(remaining, notifiers.cwd.notified()).await);
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

/// Wait until no PTY output arrives for `quiet_ms` duration for the given session.
///
/// The global output notifier fires for all sessions, so after each
/// notification we check whether the output was for our session by comparing
/// `last_output_at`. Only output belonging to `session_id` resets the quiet
/// timer.
async fn handle_wait_idle(
    session_id: SessionId,
    quiet_ms: u64,
    timeout_ms: u64,
    notifiers: &Arc<WaitNotifiers>,
    state: &SharedState,
) -> DaemonResponse {
    let quiet = Duration::from_millis(quiet_ms);
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    // Record the last output time for this session at the start of the wait.
    // When the notifier fires, we compare against this to detect new output.
    let mut last_seen = {
        let guard = state.lock().await;
        guard.sessions.get(&session_id).and_then(|s| s.last_output_at)
    };

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return DaemonResponse::Error {
                message: format!("timed out waiting for {session_id} to be idle for {quiet_ms}ms"),
            };
        }

        // Wait for the quiet period or a global output notification.
        let wait_time = quiet.min(remaining);
        let result = tokio::time::timeout(wait_time, notifiers.output.notified()).await;
        if result.is_err() {
            // Timed out waiting — no output for wait_time. Idle achieved.
            return DaemonResponse::Ok;
        }

        // A notification fired. Check whether it belongs to our session.
        let current = {
            let guard = state.lock().await;
            guard.sessions.get(&session_id).and_then(|s| s.last_output_at)
        };

        if current != last_seen {
            // New output for our session — reset the quiet timer by updating
            // last_seen and looping; the next iteration will wait another
            // full quiet period.
            last_seen = current;
        }
        // If current == last_seen, the output was from a different session;
        // the quiet period is uninterrupted for our session — loop and wait
        // again for the remaining quiet duration.
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

/// Compare two screens' dimensions, cell content, and cursor position,
/// returning the first mismatch as a human-readable message.
///
/// Only the reference's non-space cells are compared: blank cells are padding a
/// producer is free to spell as either a space or an empty cell.
fn compare_screen_content(current: &ScreenSnapshot, reference: &ScreenSnapshot) -> Option<String> {
    if current.cols != reference.cols || current.rows != reference.rows {
        return Some(format!(
            "snapshot size mismatch: current {}x{}, reference {}x{}",
            current.cols, current.rows, reference.cols, reference.rows,
        ));
    }

    for (i, (cur, refr)) in current.cells.iter().zip(reference.cells.iter()).enumerate() {
        if refr.c != ' ' && cur.c != refr.c {
            let cols = usize::from(current.cols).max(1);
            let row = i / cols;
            let col = i % cols;
            return Some(format!(
                "cell ({row},{col}): expected '{}' but found '{}'",
                refr.c, cur.c
            ));
        }
    }

    if current.cursor_row != reference.cursor_row || current.cursor_col != reference.cursor_col {
        return Some(format!(
            "cursor position mismatch: current ({},{}), reference ({},{})",
            current.cursor_row, current.cursor_col, reference.cursor_row, reference.cursor_col,
        ));
    }

    None
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

    if let Some(message) = compare_screen_content(&current, reference) {
        return DaemonResponse::AssertFailed { message };
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

/// Assert that no zero-byte `PtyOutput` frame ever arrived for a session.
///
/// The server drops a chunk its filters emptied before framing it, so any
/// count above zero means an empty frame travelled the whole IPC pipeline.
async fn handle_assert_no_empty_output(
    session_id: SessionId,
    state: &SharedState,
) -> DaemonResponse {
    let guard = state.lock().await;
    let Some(session) = guard.sessions.get(&session_id) else {
        return DaemonResponse::Error { message: format!("unknown session: {session_id}") };
    };

    match session.empty_output_frames {
        0 => DaemonResponse::Ok,
        count => DaemonResponse::AssertFailed {
            message: format!("received {count} empty PtyOutput frame(s) for session {session_id}"),
        },
    }
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

/// What an exit assertion is waiting for: a normal exit status, or the signal
/// that terminated the child.
#[derive(Clone, Copy)]
enum ExpectedExit {
    Code(i32),
    Signal(i32),
}

impl ExpectedExit {
    /// Compare an observed wait status against this expectation, returning the
    /// mismatch text when it does not hold.
    fn mismatch(self, observed: ChildExit) -> Option<String> {
        match self {
            Self::Code(expected) => match (observed.exit_code, observed.signal) {
                (Some(code), _) if code == expected => None,
                (Some(code), _) => Some(format!("expected exit code {expected} but got {code}")),
                (None, Some(signal)) => {
                    Some(format!("expected exit code {expected} but child died on signal {signal}"))
                }
                (None, None) => {
                    Some(format!("expected exit code {expected} but no wait status was reported"))
                }
            },
            Self::Signal(expected) => match (observed.signal, observed.exit_code) {
                (Some(signal), _) if signal == expected => None,
                (Some(signal), _) => Some(format!("expected signal {expected} but got {signal}")),
                (None, Some(code)) => {
                    Some(format!("expected signal {expected} but child exited with code {code}"))
                }
                (None, None) => {
                    Some(format!("expected signal {expected} but no wait status was reported"))
                }
            },
        }
    }
}

/// Assert that a session exited with the expected code or terminating signal,
/// on exactly one `SessionExited` frame.
async fn handle_assert_exit(
    session_id: SessionId,
    expected: ExpectedExit,
    timeout_ms: u64,
    state: &SharedState,
    notifiers: &Arc<WaitNotifiers>,
) -> DaemonResponse {
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let exit_status = check_exit_status(session_id, state).await;
        match exit_status {
            Some(SessionStatus::Exited(observed)) => {
                if let Some(message) = expected.mismatch(observed) {
                    return DaemonResponse::AssertFailed { message };
                }
                if observed.frames != 1 {
                    return DaemonResponse::AssertFailed {
                        message: format!(
                            "SessionExited fired {} times for {session_id}, expected exactly once",
                            observed.frames
                        ),
                    };
                }
                return DaemonResponse::Ok;
            }
            Some(SessionStatus::Running) | None => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return DaemonResponse::Error {
                        message: format!("timed out waiting for {session_id} to exit"),
                    };
                }
                drop(tokio::time::timeout(remaining, notifiers.exit.notified()).await);
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

/// Report the window id the server assigned this daemon.
///
/// The `Welcome` arrives on the reader task immediately after the daemon's
/// `Hello`, before its socket is bound, so a caller that reached the daemon at
/// all normally sees it. An absent id is an error rather than a placeholder: a
/// caller (the visual rig's entrypoint) is about to hand it to a client as a
/// join target, and joining "no window" silently would reintroduce the empty
/// window this whole path exists to avoid.
async fn handle_window_id(state: &SharedState) -> DaemonResponse {
    state.lock().await.window_id.map_or_else(
        || DaemonResponse::Error { message: "daemon has not received a Welcome yet".to_owned() },
        |window_id| DaemonResponse::WindowId { window_id },
    )
}

/// Report the launch (env-envelope) id the daemon minted for a session it
/// created. Attached sessions have none — the id belongs to whichever client
/// issued the `CreateSession`.
async fn handle_envelope_id(session_id: SessionId, state: &SharedState) -> DaemonResponse {
    state.lock().await.envelope_ids.get(&session_id).cloned().map_or_else(
        || DaemonResponse::Error {
            message: format!("no envelope id recorded for session {session_id}"),
        },
        |envelope_id| DaemonResponse::EnvelopeId { envelope_id },
    )
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
