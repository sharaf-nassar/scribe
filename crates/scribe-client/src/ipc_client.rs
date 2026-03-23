//! IPC client connecting to the scribe-server over a Unix socket.
//!
//! Supports multiple concurrent sessions: each pane can create its own
//! session and route keyboard input independently by session ID.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use scribe_common::ai_state::AiProcessState;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::socket::server_socket_path;
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;
use winit::event_loop::EventLoopProxy;

/// Commands sent from the winit main thread to the IPC background thread.
#[derive(Debug)]
pub enum ClientCommand {
    /// Raw bytes produced by a key press, routed to a specific session.
    KeyInput { session_id: SessionId, data: Vec<u8> },
    /// PTY resize notification for a specific session.
    Resize { session_id: SessionId, cols: u16, rows: u16 },
    /// Create a new session in the given workspace.
    CreateSession { workspace_id: WorkspaceId },
    /// Close a session.
    CloseSession { session_id: SessionId },
    /// Subscribe to output from additional sessions.
    Subscribe { session_ids: Vec<SessionId> },
}

/// Events forwarded from the IPC background thread to the winit event loop.
#[derive(Debug)]
pub enum UiEvent {
    /// Raw PTY output bytes for a specific session.
    PtyOutput { session_id: SessionId, data: Vec<u8> },
    /// The server has acknowledged session creation.
    SessionCreated {
        session_id: SessionId,
        #[allow(dead_code, reason = "workspace_id preserved for future workspace management")]
        workspace_id: WorkspaceId,
    },
    /// A session has exited.
    SessionExited {
        session_id: SessionId,
        #[allow(dead_code, reason = "exit_code preserved for future status display")]
        exit_code: Option<i32>,
    },
    /// The AI state for a session has changed.
    AiStateChanged { session_id: SessionId, ai_state: AiProcessState },
    /// The connection to the server was lost.
    ServerDisconnected,
    /// Animation timer tick -- sent by the animation thread to drive redraws.
    AnimationTick,
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
pub fn start_ipc_thread(proxy: EventLoopProxy<UiEvent>) -> mpsc::Sender<ClientCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    // Wrap cmd_rx in Arc<Mutex<_>> so it can be moved into spawn_blocking
    // closures which require 'static bounds.
    let cmd_rx = Arc::new(Mutex::new(cmd_rx));

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
            Ok(ServerMessage::SessionCreated { session_id, workspace_id }) => {
                tracing::debug!(session = %session_id, "session created via server response");
                send_event(&proxy, UiEvent::SessionCreated { session_id, workspace_id });
            }
            Ok(ServerMessage::AiStateChanged { session_id, ai_state }) => {
                send_event(&proxy, UiEvent::AiStateChanged { session_id, ai_state });
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
        ClientCommand::KeyInput { session_id, data } => {
            ClientMessage::KeyInput { session_id, data }
        }
        ClientCommand::Resize { session_id, cols, rows } => {
            ClientMessage::Resize { session_id, cols, rows }
        }
        ClientCommand::CreateSession { workspace_id } => {
            ClientMessage::CreateSession { workspace_id }
        }
        ClientCommand::CloseSession { session_id } => ClientMessage::CloseSession { session_id },
        ClientCommand::Subscribe { session_ids } => ClientMessage::Subscribe { session_ids },
    }
}

/// Async entry point running on the background thread's Tokio runtime.
///
/// Connects to the server and then drives the read and write halves
/// concurrently until the connection is closed.
///
/// Session creation is initiated by the UI thread via `ClientCommand::CreateSession`
/// rather than during the IPC handshake, ensuring exactly one session per pane.
async fn ipc_main(
    proxy: EventLoopProxy<UiEvent>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ClientCommand>>>,
) {
    let socket_path = server_socket_path();

    let stream = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(?socket_path, error = %e, "failed to connect to scribe server");
            send_event(&proxy, UiEvent::ServerDisconnected);
            return;
        }
    };

    let (reader, writer) = stream.into_split();

    let read_proxy = proxy.clone();
    let read_task = tokio::spawn(run_read_task(reader, read_proxy));
    let write_task = tokio::spawn(run_write_task(writer, cmd_rx));

    // Drive both tasks to completion.
    drop(tokio::join!(read_task, write_task));
}
