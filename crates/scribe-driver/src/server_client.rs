//! IPC client connecting scribe-driver to the scribe-server over a Unix socket.
//!
//! The driver process is not a UI window, so it identifies itself with
//! `Hello { window_id: None }` and only exchanges driver-task messages.
//!
//! # Thread model
//!
//! [`start_server_client`] spawns a single OS thread that owns a Tokio runtime.
//! Within that runtime two tasks run concurrently:
//!
//! - **Read task** — reads [`ServerMessage`] frames, maps driver-task variants
//!   to [`DriverServerEvent`], and forwards them to the main thread via a
//!   user-supplied callback.
//! - **Write task** — receives [`DriverServerCommand`] from the returned
//!   [`std::sync::mpsc::Sender`], maps them to [`ClientMessage`] frames, and
//!   writes them to the socket.
//!
//! Clean separation: no `SQLite`, no webview concerns here.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use scribe_common::ai_state::AiProcessState;
use scribe_common::driver::{DriverTaskInfo, DriverTaskState};
use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::socket::server_socket_path;
use tokio::io::AsyncWriteExt as _;

// ── Public types ──────────────────────────────────────────────────────────────

/// Commands sent from the main (GTK/tao) thread to the IPC background thread.
#[derive(Debug)]
pub enum DriverServerCommand {
    /// Create a new driver task on the server.
    CreateTask { task_id: uuid::Uuid, project_path: PathBuf, description: String },
    /// Stop a running driver task.
    StopTask { task_id: uuid::Uuid },
    /// Send raw input bytes to a driver task's PTY.
    SendInput { task_id: uuid::Uuid, data: Vec<u8> },
    /// Request a list of all live driver tasks.
    ListTasks,
    /// Attach to an existing driver task to receive its output stream.
    AttachTask { task_id: uuid::Uuid },
}

/// Events received from the server, forwarded to the main thread.
#[derive(Debug)]
pub enum DriverServerEvent {
    /// The server confirmed that a driver task was created.
    TaskCreated { task_id: uuid::Uuid, project_path: PathBuf },
    /// Raw PTY output bytes for a driver task.
    TaskOutput { task_id: uuid::Uuid, data: Vec<u8> },
    /// The lifecycle state (or AI sub-state) of a driver task changed.
    TaskStateChanged {
        task_id: uuid::Uuid,
        state: DriverTaskState,
        ai_state: Option<AiProcessState>,
    },
    /// Current snapshot of all live driver tasks (response to `ListTasks`).
    TaskList { tasks: Vec<DriverTaskInfo> },
    /// A driver task's PTY process has exited.
    TaskExited { task_id: uuid::Uuid, exit_code: Option<i32> },
    /// The connection to the server was lost.
    ConnectionLost,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the server IPC client on a background thread.
///
/// Spawns an OS thread with its own single-threaded Tokio runtime. The thread
/// connects to the scribe-server socket, sends an initial `Hello` + `ListDriverTasks`,
/// then drives the read/write tasks concurrently.
///
/// `event_cb` is called from the background thread whenever a
/// [`DriverServerEvent`] arrives. It must be `Send + 'static`.
///
/// Returns an [`mpsc::Sender<DriverServerCommand>`] the main thread can use to
/// send commands to the server.
pub fn start_server_client<F>(event_cb: F) -> mpsc::Sender<DriverServerCommand>
where
    F: Fn(DriverServerEvent) + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::channel::<DriverServerCommand>();
    let cmd_rx = Arc::new(Mutex::new(cmd_rx));

    // Queue Hello as the first command so it is the first message on the wire.
    if cmd_tx.send(DriverServerCommand::ListTasks).is_err() {
        tracing::warn!("IPC channel closed before initial ListTasks could be queued");
    }

    std::thread::spawn(move || {
        #[allow(
            clippy::expect_used,
            reason = "Tokio runtime creation in dedicated background thread; failure is unrecoverable"
        )]
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(ipc_main(event_cb, cmd_rx));
    });

    cmd_tx
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Forward a [`DriverServerEvent`] via the callback, logging if it panics.
fn send_event<F>(cb: &F, event: DriverServerEvent)
where
    F: Fn(DriverServerEvent),
{
    cb(event);
}

/// Drive the read half: map [`ServerMessage`] driver-task variants to
/// [`DriverServerEvent`] and forward them to the main thread.
async fn run_read_task<F>(mut reader: tokio::net::unix::OwnedReadHalf, cb: F)
where
    F: Fn(DriverServerEvent),
{
    loop {
        match read_message::<ServerMessage, _>(&mut reader).await {
            Ok(ServerMessage::DriverTaskCreated { task_id, project_path }) => {
                send_event(&cb, DriverServerEvent::TaskCreated { task_id, project_path });
            }
            Ok(ServerMessage::DriverTaskOutput { task_id, data }) => {
                send_event(&cb, DriverServerEvent::TaskOutput { task_id, data });
            }
            Ok(ServerMessage::DriverTaskStateChanged { task_id, state, ai_state }) => {
                send_event(&cb, DriverServerEvent::TaskStateChanged { task_id, state, ai_state });
            }
            Ok(ServerMessage::DriverTaskList { tasks }) => {
                send_event(&cb, DriverServerEvent::TaskList { tasks });
            }
            Ok(ServerMessage::DriverTaskExited { task_id, exit_code }) => {
                tracing::info!(%task_id, ?exit_code, "driver task exited");
                send_event(&cb, DriverServerEvent::TaskExited { task_id, exit_code });
            }
            Ok(other) => {
                // Non-driver messages are not relevant to the driver process.
                tracing::debug!(?other, "ignoring non-driver server message");
            }
            Err(e) => {
                tracing::warn!(error = %e, "server read error; connection lost");
                send_event(&cb, DriverServerEvent::ConnectionLost);
                break;
            }
        }
    }
}

/// Drive the write half: receive [`DriverServerCommand`] from the main thread
/// and forward them as [`ClientMessage`] frames to the server.
async fn run_write_task(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    cmd_rx: Arc<Mutex<mpsc::Receiver<DriverServerCommand>>>,
) {
    loop {
        let rx_clone = Arc::<Mutex<mpsc::Receiver<DriverServerCommand>>>::clone(&cmd_rx);

        let recv_result = tokio::task::spawn_blocking(move || {
            rx_clone.lock().map_err(|_| ()).and_then(|guard| guard.recv().map_err(|_| ()))
        })
        .await;

        let Ok(Ok(cmd)) = recv_result else {
            // Sender dropped, mutex poisoned, or JoinError — channel closed.
            break;
        };

        let msg = command_to_message(cmd);

        if let Err(e) = write_message(&mut writer, &msg).await {
            tracing::warn!(error = %e, "server write error; closing connection");
            break;
        }
    }

    // Best-effort flush before the writer is dropped.
    if let Err(e) = writer.flush().await {
        tracing::debug!(error = %e, "flush on write task exit failed");
    }
}

/// Map a [`DriverServerCommand`] to the corresponding [`ClientMessage`].
fn command_to_message(cmd: DriverServerCommand) -> ClientMessage {
    match cmd {
        DriverServerCommand::CreateTask { task_id, project_path, description } => {
            ClientMessage::CreateDriverTask { task_id, project_path, description }
        }
        DriverServerCommand::StopTask { task_id } => ClientMessage::StopDriverTask { task_id },
        DriverServerCommand::SendInput { task_id, data } => {
            ClientMessage::DriverTaskInput { task_id, data }
        }
        DriverServerCommand::ListTasks => ClientMessage::ListDriverTasks,
        DriverServerCommand::AttachTask { task_id } => ClientMessage::AttachDriverTask { task_id },
    }
}

/// Async entry point for the background thread's Tokio runtime.
///
/// Connects to the server, sends `Hello` + `ListDriverTasks`, then drives the
/// read and write tasks concurrently until one of them exits.
async fn ipc_main<F>(cb: F, cmd_rx: Arc<Mutex<mpsc::Receiver<DriverServerCommand>>>)
where
    F: Fn(DriverServerEvent) + Send + 'static,
{
    let socket_path = server_socket_path();

    let stream = match tokio::net::UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, path = %socket_path.display(), "failed to connect to scribe-server");
            send_event(&cb, DriverServerEvent::ConnectionLost);
            return;
        }
    };

    let (reader, mut writer) = stream.into_split();

    // Identify ourselves to the server.  The driver is not a UI window, so
    // `window_id` is `None`.
    let hello = ClientMessage::Hello { window_id: None };
    if let Err(e) = write_message(&mut writer, &hello).await {
        tracing::error!(error = %e, "failed to send Hello to scribe-server");
        send_event(&cb, DriverServerEvent::ConnectionLost);
        return;
    }

    // Immediately request the current task list so the driver's in-memory
    // state is populated before any UI interaction.
    let list_req = ClientMessage::ListDriverTasks;
    if let Err(e) = write_message(&mut writer, &list_req).await {
        tracing::error!(error = %e, "failed to send ListDriverTasks to scribe-server");
        send_event(&cb, DriverServerEvent::ConnectionLost);
        return;
    }

    let read_task = tokio::spawn(run_read_task(reader, cb));
    let write_task = tokio::spawn(run_write_task(writer, cmd_rx));

    // When either task finishes, abort the other.
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
