//! IPC client connecting to the scribe-server over a Unix socket.
//!
//! Supports multiple concurrent sessions: each pane can create its own
//! session and route keyboard input independently by session ID.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use scribe_common::ai_state::AiProcessState;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    AutomationAction, ClientMessage, PromptMarkKind, SearchMatch, ServerMessage, TerminalSize,
    UpdateProgressState,
};
use scribe_common::socket::server_socket_path;
use tokio::io::AsyncWriteExt as _;
use winit::event_loop::EventLoopProxy;

/// Commands sent from the winit main thread to the IPC background thread.
#[derive(Debug)]
pub enum ClientCommand {
    /// Raw bytes produced by a key press, routed to a specific session.
    KeyInput { session_id: SessionId, data: Vec<u8>, dismisses_attention: bool },
    /// PTY resize notification for a specific session.
    Resize { session_id: SessionId, size: TerminalSize },
    /// Create a new session in the given workspace.
    ///
    /// When `split_direction` is `Some`, the server records the layout
    /// direction so it can be sent back on reconnect.
    CreateSession {
        workspace_id: WorkspaceId,
        split_direction: Option<scribe_common::protocol::LayoutDirection>,
        cwd: Option<std::path::PathBuf>,
        size: Option<TerminalSize>,
        command: Option<Vec<String>>,
    },
    /// Close a session.
    CloseSession { session_id: SessionId },
    /// Subscribe to output from additional sessions.
    Subscribe { session_ids: Vec<SessionId> },
    /// Request a list of all live sessions on the server.
    ListSessions,
    /// Attach to existing (detached) sessions on the server.
    AttachSessions { session_ids: Vec<SessionId>, dimensions: Vec<TerminalSize> },
    /// Notify server that config file has been updated.
    ConfigReloaded,
    /// Report the current workspace split tree to the server.
    ReportWorkspaceTree { tree: scribe_common::protocol::WorkspaceTreeNode },
    /// Identify this client window to the server (sent as first message).
    Hello { window_id: Option<WindowId> },
    /// Close this window and destroy all its sessions on the server.
    CloseWindow { window_id: WindowId },
    /// Request all clients to save state and quit.
    QuitAll,
    /// User confirmed update — download and install.
    TriggerUpdate,
    /// User dismissed update notification.
    DismissUpdate,
    /// Notify server of pane focus change for CSI focus events.
    FocusChanged { gained: Option<SessionId>, lost: Option<SessionId> },
    /// Request a scrollback snapshot at a given offset from the bottom.
    #[allow(
        dead_code,
        reason = "server-backed scroll snapshots are implemented ahead of the client UX that consumes them"
    )]
    ScrollRequest { session_id: SessionId, offset: i32 },
    /// Search the terminal scrollback/screen.
    SearchRequest { session_id: SessionId, query: String, limit: u32 },
}

/// Events forwarded from the IPC background thread to the winit event loop.
#[derive(Debug)]
pub enum UiEvent {
    /// Raw PTY output bytes for a specific session.
    PtyOutput { session_id: SessionId, data: Vec<u8> },
    /// Full screen snapshot for restoring visible content on reconnect.
    ScreenSnapshot { session_id: SessionId, snapshot: scribe_common::screen::ScreenSnapshot },
    /// The server has acknowledged session creation.
    SessionCreated {
        session_id: SessionId,
        #[allow(dead_code, reason = "workspace_id preserved for future workspace management")]
        workspace_id: WorkspaceId,
        shell_name: String,
    },
    /// A session has exited.
    SessionExited {
        session_id: SessionId,
        #[allow(dead_code, reason = "exit_code preserved for future status display")]
        exit_code: Option<i32>,
    },
    /// The AI state for a session has changed.
    AiStateChanged { session_id: SessionId, ai_state: AiProcessState },
    /// The AI state for a session was explicitly cleared.
    AiStateCleared { session_id: SessionId },
    /// The terminal emitted BEL for a session.
    Bell { session_id: SessionId },
    /// The working directory for a session has changed.
    CwdChanged { session_id: SessionId, cwd: PathBuf },
    /// The shell/session context for a session has changed.
    SessionContextChanged {
        session_id: SessionId,
        context: scribe_common::protocol::SessionContext,
    },
    /// The terminal title for a session has changed.
    TitleChanged { session_id: SessionId, title: String },
    /// The active Codex task label for a session has changed.
    CodexTaskLabelChanged { session_id: SessionId, task_label: String },
    /// The active Codex task label for a session was cleared.
    CodexTaskLabelCleared { session_id: SessionId },
    /// Git branch for a session's CWD (None if not in a git repo).
    GitBranch {
        session_id: SessionId,
        #[allow(dead_code, reason = "branch preserved for future status bar display")]
        branch: Option<String>,
    },
    /// Full workspace state sent from the server.
    WorkspaceInfo {
        workspace_id: WorkspaceId,
        name: Option<String>,
        accent_color: String,
        split_direction: Option<scribe_common::protocol::LayoutDirection>,
    },
    /// List of all live sessions, received in response to `ListSessions`.
    SessionList {
        sessions: Vec<scribe_common::protocol::SessionInfo>,
        workspace_tree: Option<scribe_common::protocol::WorkspaceTreeNode>,
    },
    /// A workspace has been auto-named.
    WorkspaceNamed { workspace_id: WorkspaceId, name: String },
    /// Server configuration has been reloaded.
    ConfigChanged,
    /// The connection to the server was lost.
    ServerDisconnected,
    /// Animation timer tick -- sent by the animation thread to drive redraws.
    AnimationTick,
    /// Server confirmed our window identity and listed other windows to spawn.
    Welcome { window_id: WindowId, other_windows: Vec<WindowId> },
    /// Server confirmed that this window was permanently removed.
    WindowClosed { window_id: WindowId },
    /// Server requested us to save state and quit (`QuitAll` was acknowledged).
    QuitRequested,
    /// Server requested that this client execute an automation action.
    RunAction { action: AutomationAction },
    /// Server found an available update.
    UpdateAvailable { version: String, release_url: String },
    /// Update progress changed.
    UpdateProgress { state: UpdateProgressState },
    /// A shell prompt-mark event from OSC 133.
    PromptMark {
        session_id: SessionId,
        kind: PromptMarkKind,
        #[allow(dead_code, reason = "click_events preserved for future click-to-move feature")]
        click_events: bool,
        #[allow(dead_code, reason = "exit_code preserved for future command status display")]
        exit_code: Option<i32>,
    },
    /// Scrollback snapshot at an offset from the bottom.
    ScrolledSnapshot {
        session_id: SessionId,
        snapshot: scribe_common::screen::ScreenSnapshot,
        applied_offset: u32,
    },
    /// Search results for the current query.
    SearchResults { session_id: SessionId, query: String, matches: Vec<SearchMatch> },
}

/// Start the IPC client on a background thread.
///
/// Spawns a `std::thread` that owns a single-threaded Tokio runtime.
/// The runtime connects to the server and bridges server messages to
/// the winit event loop via `proxy`, while routing keyboard / resize /
/// session commands received on the returned sender to the server.
///
/// Returns an [`mpsc::Sender<ClientCommand>`] that the main thread can
/// use to forward keyboard input, resize events, and session commands.
pub fn start_ipc_thread(
    proxy: EventLoopProxy<UiEvent>,
    window_id: Option<WindowId>,
) -> mpsc::Sender<ClientCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    // Wrap cmd_rx in Arc<Mutex<_>> so it can be moved into spawn_blocking
    // closures which require 'static bounds.
    let cmd_rx = Arc::new(Mutex::new(cmd_rx));

    // Send Hello as the first command so it's the first message on the wire.
    if cmd_tx.send(ClientCommand::Hello { window_id }).is_err() {
        tracing::warn!("IPC channel closed before Hello could be sent");
    }

    std::thread::spawn(move || {
        #[allow(
            clippy::expect_used,
            reason = "runtime creation in thread spawn setup is infallible in practice"
        )]
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(ipc_main(proxy, cmd_rx));
    });

    cmd_tx
}

/// Send a `UiEvent` via the event loop proxy, logging if the event loop is gone.
fn send_event(proxy: &EventLoopProxy<UiEvent>, event: UiEvent) {
    if proxy.send_event(event).is_err() {
        tracing::warn!("winit event loop closed; dropping event");
    }
}

/// Drive the read half: forward server messages to the winit event loop.
#[allow(
    clippy::too_many_lines,
    reason = "flat sequential match arms for all server message variants"
)]
async fn run_read_task(
    mut reader: tokio::net::unix::OwnedReadHalf,
    proxy: EventLoopProxy<UiEvent>,
) {
    loop {
        match read_message::<ServerMessage, _>(&mut reader).await {
            Ok(ServerMessage::PtyOutput { session_id, data }) => {
                send_event(&proxy, UiEvent::PtyOutput { session_id, data });
            }
            Ok(ServerMessage::SessionExited { session_id, exit_code }) => {
                tracing::info!(session = %session_id, ?exit_code, "session exited");
                send_event(&proxy, UiEvent::SessionExited { session_id, exit_code });
            }
            Ok(ServerMessage::ScreenSnapshot { session_id, snapshot }) => {
                send_event(&proxy, UiEvent::ScreenSnapshot { session_id, snapshot });
            }
            Ok(ServerMessage::SessionCreated { session_id, workspace_id, shell_name }) => {
                tracing::debug!(session = %session_id, "session created via server response");
                send_event(
                    &proxy,
                    UiEvent::SessionCreated { session_id, workspace_id, shell_name },
                );
            }
            Ok(ServerMessage::AiStateChanged { session_id, ai_state }) => {
                send_event(&proxy, UiEvent::AiStateChanged { session_id, ai_state });
            }
            Ok(ServerMessage::AiStateCleared { session_id }) => {
                send_event(&proxy, UiEvent::AiStateCleared { session_id });
            }
            Ok(ServerMessage::Bell { session_id }) => {
                send_event(&proxy, UiEvent::Bell { session_id });
            }
            Ok(ServerMessage::CwdChanged { session_id, cwd }) => {
                send_event(&proxy, UiEvent::CwdChanged { session_id, cwd });
            }
            Ok(ServerMessage::SessionContextChanged { session_id, context }) => {
                send_event(&proxy, UiEvent::SessionContextChanged { session_id, context });
            }
            Ok(ServerMessage::TitleChanged { session_id, title }) => {
                send_event(&proxy, UiEvent::TitleChanged { session_id, title });
            }
            Ok(ServerMessage::CodexTaskLabelChanged { session_id, task_label }) => {
                send_event(&proxy, UiEvent::CodexTaskLabelChanged { session_id, task_label });
            }
            Ok(ServerMessage::CodexTaskLabelCleared { session_id }) => {
                send_event(&proxy, UiEvent::CodexTaskLabelCleared { session_id });
            }
            Ok(ServerMessage::GitBranch { session_id, branch }) => {
                send_event(&proxy, UiEvent::GitBranch { session_id, branch });
            }
            Ok(ServerMessage::WorkspaceInfo {
                workspace_id,
                name,
                accent_color,
                split_direction,
            }) => {
                send_event(
                    &proxy,
                    UiEvent::WorkspaceInfo { workspace_id, name, accent_color, split_direction },
                );
            }
            Ok(ServerMessage::SessionList { sessions, workspace_tree }) => {
                send_event(&proxy, UiEvent::SessionList { sessions, workspace_tree });
            }
            Ok(ServerMessage::WorkspaceNamed { workspace_id, name }) => {
                send_event(&proxy, UiEvent::WorkspaceNamed { workspace_id, name });
            }
            Ok(ServerMessage::Welcome { window_id, other_windows }) => {
                tracing::info!(%window_id, others = other_windows.len(), "received Welcome");
                send_event(&proxy, UiEvent::Welcome { window_id, other_windows });
            }
            Ok(ServerMessage::WindowClosed { window_id }) => {
                tracing::info!(%window_id, "received WindowClosed from server");
                send_event(&proxy, UiEvent::WindowClosed { window_id });
            }
            Ok(ServerMessage::QuitRequested) => {
                tracing::info!("received QuitRequested from server");
                send_event(&proxy, UiEvent::QuitRequested);
            }
            Ok(ServerMessage::RunAction { action }) => {
                tracing::info!(?action, "received RunAction from server");
                send_event(&proxy, UiEvent::RunAction { action });
            }
            Ok(ServerMessage::ActionDispatched { window_id }) => {
                tracing::debug!(%window_id, "ignoring ActionDispatched on UI client connection");
            }
            Ok(ServerMessage::UpdateAvailable { version, release_url }) => {
                tracing::info!(%version, "update available");
                send_event(&proxy, UiEvent::UpdateAvailable { version, release_url });
            }
            Ok(ServerMessage::UpdateProgress { state }) => {
                send_event(&proxy, UiEvent::UpdateProgress { state });
            }
            Ok(ServerMessage::PromptMark { session_id, kind, click_events, exit_code }) => {
                send_event(
                    &proxy,
                    UiEvent::PromptMark { session_id, kind, click_events, exit_code },
                );
            }
            Ok(ServerMessage::ScrolledSnapshot { session_id, snapshot, applied_offset }) => {
                send_event(
                    &proxy,
                    UiEvent::ScrolledSnapshot { session_id, snapshot, applied_offset },
                );
            }
            Ok(ServerMessage::SearchResults { session_id, query, matches }) => {
                send_event(&proxy, UiEvent::SearchResults { session_id, query, matches });
            }
            Ok(other) => {
                tracing::debug!(?other, "unhandled server message");
            }
            Err(e) => {
                tracing::warn!(error = %e, "server read error; closing connection");
                send_event(&proxy, UiEvent::ServerDisconnected);
                break;
            }
        }
    }
}

/// Drive the write half: receive commands from the main thread and forward
/// them to the server.
async fn run_write_task(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ClientCommand>>>,
    proxy: EventLoopProxy<UiEvent>,
) {
    loop {
        // Clone the Arc so the spawn_blocking closure owns its reference.
        let rx_clone = Arc::<Mutex<mpsc::Receiver<ClientCommand>>>::clone(&cmd_rx);

        // Bridge the blocking recv() call into async.
        let recv_result = tokio::task::spawn_blocking(move || {
            rx_clone.lock().map_err(|_| ()).and_then(|guard| guard.recv().map_err(|_| ()))
        })
        .await;

        let Ok(Ok(cmd)) = recv_result else {
            // Sender dropped, mutex poisoned, or JoinError -- channel closed.
            break;
        };

        let msg = command_to_message(cmd);

        if let Err(e) = write_message(&mut writer, &msg).await {
            tracing::warn!(error = %e, "server write error; closing connection");
            send_event(&proxy, UiEvent::ServerDisconnected);
            break;
        }
    }

    // Best-effort flush before dropping the writer.
    if let Err(e) = writer.flush().await {
        tracing::debug!(error = %e, "flush on write task exit failed");
    }
}

/// Convert a `ClientCommand` to a `ClientMessage` for the wire.
fn command_to_message(cmd: ClientCommand) -> ClientMessage {
    match cmd {
        ClientCommand::KeyInput { session_id, data, dismisses_attention } => {
            ClientMessage::KeyInput { session_id, data, dismisses_attention }
        }
        ClientCommand::Resize { session_id, size } => ClientMessage::Resize { session_id, size },
        ClientCommand::CreateSession { workspace_id, split_direction, cwd, size, command } => {
            ClientMessage::CreateSession { workspace_id, split_direction, cwd, size, command }
        }
        ClientCommand::CloseSession { session_id } => ClientMessage::CloseSession { session_id },
        ClientCommand::Subscribe { session_ids } => ClientMessage::Subscribe { session_ids },
        ClientCommand::ListSessions => ClientMessage::ListSessions,
        ClientCommand::AttachSessions { session_ids, dimensions } => {
            ClientMessage::AttachSessions { session_ids, dimensions }
        }
        ClientCommand::ConfigReloaded => ClientMessage::ConfigReloaded,
        ClientCommand::ReportWorkspaceTree { tree } => ClientMessage::ReportWorkspaceTree { tree },
        ClientCommand::Hello { window_id } => ClientMessage::Hello { window_id },
        ClientCommand::CloseWindow { window_id } => ClientMessage::CloseWindow { window_id },
        ClientCommand::QuitAll => ClientMessage::QuitAll,
        ClientCommand::TriggerUpdate => ClientMessage::TriggerUpdate,
        ClientCommand::DismissUpdate => ClientMessage::DismissUpdate,
        ClientCommand::FocusChanged { gained, lost } => {
            ClientMessage::FocusChanged { gained, lost }
        }
        ClientCommand::ScrollRequest { session_id, offset } => {
            ClientMessage::ScrollRequest { session_id, offset }
        }
        ClientCommand::SearchRequest { session_id, query, limit } => {
            ClientMessage::SearchRequest { session_id, query, limit }
        }
    }
}

/// Async entry point running on the background thread's Tokio runtime.
///
/// Connects to the server and then drives the read and write halves
/// concurrently until the connection is closed.
///
/// Session creation is initiated by the UI thread via `ClientCommand::CreateSession`
/// rather than during the IPC handshake, ensuring exactly one session per pane.
/// Maximum time to wait for the server to become ready after starting the service.
const SERVER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Interval between connection retry attempts while waiting for the service.
const SERVER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Start the scribe-server process.
///
/// On Linux, uses the systemd user service. On macOS, spawns the binary
/// directly as a detached background process.
fn start_server() -> Result<(), String> {
    platform_start_server()
}

#[cfg(target_os = "linux")]
fn platform_start_server() -> Result<(), String> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", "scribe-server"])
        .status()
        .map_err(|e| format!("failed to run systemctl: {e}"))?;
    if status.success() {
        tracing::info!("scribe-server.service started");
        Ok(())
    } else {
        Err(format!("systemctl start exited with {status}"))
    }
}

#[cfg(target_os = "macos")]
fn platform_start_server() -> Result<(), String> {
    match start_server_via_launchctl() {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("launchctl start failed ({e}), falling back to direct spawn");
            start_server_directly()
        }
    }
}

#[cfg(target_os = "macos")]
fn start_server_via_launchctl() -> Result<(), String> {
    let uid = scribe_common::socket::current_uid();
    let domain = format!("user/{uid}");
    let service = format!("user/{uid}/com.scribe.server");

    let home = std::env::var("HOME").map_err(|e| format!("HOME not set: {e}"))?;
    let agents_dir = std::path::PathBuf::from(&home).join("Library/LaunchAgents");
    let installed_plist = agents_dir.join("com.scribe.server.plist");

    if !installed_plist.exists() {
        let exe = std::env::current_exe().map_err(|e| format!("failed to get current exe: {e}"))?;
        // In a .app bundle: Contents/MacOS/scribe-client → parent = MacOS → parent = Contents
        let contents_dir = exe.parent().and_then(|p| p.parent()).ok_or_else(|| {
            String::from("could not resolve Contents directory from executable path")
        })?;
        let bundled_plist = contents_dir.join("Resources/com.scribe.server.plist");

        if !bundled_plist.exists() {
            return Err(format!("bundled plist not found at {}", bundled_plist.display()));
        }

        std::fs::create_dir_all(&agents_dir)
            .map_err(|e| format!("failed to create LaunchAgents dir: {e}"))?;
        std::fs::copy(&bundled_plist, &installed_plist)
            .map_err(|e| format!("failed to copy plist: {e}"))?;
        tracing::info!(plist = %installed_plist.display(), "installed launchd agent plist");

        let status = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain, installed_plist.to_str().unwrap_or("")])
            .status()
            .map_err(|e| format!("failed to run launchctl bootstrap: {e}"))?;
        if !status.success() {
            return Err(format!("launchctl bootstrap exited with {status}"));
        }
        tracing::info!(%service, "bootstrapped launchd agent");
    }

    let status = std::process::Command::new("launchctl")
        .args(["kickstart", &service])
        .status()
        .map_err(|e| format!("failed to run launchctl kickstart: {e}"))?;
    if !status.success() {
        return Err(format!("launchctl kickstart exited with {status}"));
    }
    tracing::info!(%service, "kickstarted launchd agent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_server_directly() -> Result<(), String> {
    use std::process::Stdio;

    // Resolve server binary relative to current executable.
    // In a .app bundle: Contents/MacOS/scribe-server
    // In dev: same directory as scribe-client
    let exe = std::env::current_exe().map_err(|e| format!("failed to get current exe: {e}"))?;
    let server_exe = exe.with_file_name("scribe-server");

    if !server_exe.exists() {
        return Err(format!("server binary not found at {}", server_exe.display()));
    }

    let child = std::process::Command::new(&server_exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn scribe-server: {e}"))?;

    tracing::info!(pid = child.id(), exe = %server_exe.display(), "spawned scribe-server");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_start_server() -> Result<(), String> {
    Err(String::from("server auto-start not supported on this platform"))
}

/// Try to connect to the server socket. If the server isn't running, start it
/// and retry until it's ready or the timeout expires.
async fn connect_or_start_server(
    socket_path: &Path,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    // First attempt — server may already be running.
    if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
        return Ok(stream);
    }

    tracing::info!("server not running, starting scribe-server");
    start_server().map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    // Wait for the socket to appear.
    let deadline = tokio::time::Instant::now() + SERVER_STARTUP_TIMEOUT;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            tracing::info!("connected to scribe-server");
            return Ok(stream);
        }

        if tokio::time::Instant::now() >= deadline {
            return Err("scribe-server did not become ready within 5s".into());
        }
    }
}

async fn ipc_main(
    proxy: EventLoopProxy<UiEvent>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ClientCommand>>>,
) {
    let socket_path = server_socket_path();

    let stream = match connect_or_start_server(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to scribe server");
            send_event(&proxy, UiEvent::ServerDisconnected);
            return;
        }
    };

    let (reader, writer) = stream.into_split();

    let read_proxy = proxy.clone();
    let write_proxy = proxy.clone();
    let read_task = tokio::spawn(run_read_task(reader, read_proxy));
    let write_task = tokio::spawn(run_write_task(writer, cmd_rx, write_proxy));

    // When either task finishes, abort the other so the process can exit.
    // Typically the write task exits first (cmd_tx dropped when the UI
    // closes), while the read task would block forever on a still-alive
    // server socket.
    let mut read_task = read_task;
    let mut write_task = write_task;
    tokio::select! {
        _ = &mut read_task => {
            write_task.abort();
        }
        _ = &mut write_task => {
            read_task.abort();
        }
    }
}
