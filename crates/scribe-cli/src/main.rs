use std::io::Write as _;

use tokio::io::{self, AsyncReadExt as _};
use tokio::net::UnixStream;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::WorkspaceId;
use scribe_common::protocol::{ClientMessage, ServerMessage};
use scribe_common::socket::server_socket_path;

/// Write raw bytes to stdout, discarding any IO errors.
///
/// Stdout write failures are acceptable in a test CLI tool.
fn write_stdout(data: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    #[allow(
        clippy::let_underscore_must_use,
        reason = "stdout write errors in a test CLI tool are intentionally discarded"
    )]
    let _ = stdout.write_all(data);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "stdout flush errors in a test CLI tool are intentionally discarded"
    )]
    let _ = stdout.flush();
}

/// Pump PTY output from the server to local stdout until the session exits or
/// the connection closes.
async fn pump_server_output<R>(reader: &mut R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let msg: Result<ServerMessage, ScribeError> = read_message(reader).await;
        match msg {
            Ok(ServerMessage::PtyOutput { data, .. }) => write_stdout(&data),
            Ok(ServerMessage::SessionExited { session_id, exit_code }) => {
                info!(%session_id, ?exit_code, "session exited");
                break;
            }
            Ok(ServerMessage::CwdChanged { session_id, cwd }) => {
                info!(%session_id, ?cwd, "CWD changed");
            }
            Ok(ServerMessage::TitleChanged { session_id, title }) => {
                info!(%session_id, %title, "title changed");
            }
            Ok(ServerMessage::AiStateChanged { session_id, ai_state }) => {
                info!(%session_id, ?ai_state, "AI state changed");
            }
            Ok(ServerMessage::WorkspaceNamed { workspace_id, name }) => {
                info!(%workspace_id, %name, "workspace named");
            }
            Ok(ServerMessage::Bell { session_id }) => {
                info!(%session_id, "bell");
            }
            Ok(ServerMessage::ScreenSnapshot { session_id, .. }) => {
                info!(%session_id, "received screen snapshot");
            }
            Ok(other) => {
                info!(?other, "server event");
            }
            Err(_) => break,
        }
    }
}

/// Read raw bytes from stdin and forward as `KeyInput` messages to the server.
async fn pump_stdin_input<W>(session_id: scribe_common::ids::SessionId, mut writer: W)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut stdin = io::stdin();
    let mut buf = [0u8; 1024];

    loop {
        let n = match stdin.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        let data = buf.get(..n).map_or_else(Vec::new, <[u8]>::to_vec);
        let msg = ClientMessage::KeyInput { session_id, data };

        if write_message(&mut writer, &msg).await.is_err() {
            break;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), ScribeError> {
    #[allow(clippy::unwrap_used, reason = "EnvFilter::new with static string cannot fail")]
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(filter).init();

    let path = server_socket_path();
    info!(?path, "connecting to scribe-server");

    let stream = UnixStream::connect(&path).await.map_err(|e| ScribeError::Io { source: e })?;

    let (mut read_half, mut write_half) = tokio::io::split(stream);

    info!("connected");

    // Create a session
    let workspace_id = WorkspaceId::new();
    let create_msg = ClientMessage::CreateSession { workspace_id, split_direction: None };
    write_message(&mut write_half, &create_msg).await?;

    // Read the SessionCreated response
    let response: ServerMessage = read_message(&mut read_half).await?;
    info!(?response, "server response");

    let session_id = match response {
        ServerMessage::SessionCreated { session_id, .. } => session_id,
        ServerMessage::Error { message } => {
            return Err(ScribeError::ProtocolError { reason: format!("server error: {message}") });
        }
        other => {
            return Err(ScribeError::ProtocolError {
                reason: format!("unexpected response: {other:?}"),
            });
        }
    };

    info!(%session_id, "session created, forwarding stdin <-> PTY output");

    // Spawn task to read PTY output from server and print to stdout
    let output_handle = tokio::spawn(async move {
        pump_server_output(&mut read_half).await;
    });

    // Read raw bytes from stdin and forward as KeyInput messages
    let stdin_handle = tokio::spawn(pump_stdin_input(session_id, write_half));

    // Wait for either task to finish — when one ends, we're done
    tokio::select! {
        _ = output_handle => {},
        _ = stdin_handle => {},
    }

    Ok(())
}
