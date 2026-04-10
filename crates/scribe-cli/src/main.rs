use std::io::Write as _;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::io::{self, AsyncReadExt as _};
use tokio::net::UnixStream;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{WindowId, WorkspaceId};
use scribe_common::profiles;
use scribe_common::protocol::{AutomationAction, ClientMessage, ServerMessage};
use scribe_common::socket::server_socket_path;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    Windows,
    Action {
        #[arg(long)]
        window: Option<WindowId>,
        #[command(subcommand)]
        action: ActionCommand,
    },
    Profile {
        #[command(subcommand)]
        action: ProfileCommand,
    },
}

#[derive(Subcommand)]
enum ActionCommand {
    OpenSettings,
    OpenFind,
    NewTab,
    NewAiTab,
    ResumeAiTab,
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    CloseTab,
    NewWindow,
    SwitchProfile { name: String },
}

#[derive(Subcommand)]
enum ProfileCommand {
    List,
    Active,
    Save {
        name: String,
    },
    Switch {
        name: String,
    },
    Export {
        name: String,
        path: PathBuf,
    },
    Import {
        name: String,
        path: PathBuf,
        #[arg(long)]
        activate: bool,
    },
}

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

fn write_line(line: &str) {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    write_stdout(&buf);
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
            Ok(ServerMessage::WorkspaceNamed { workspace_id, name, .. }) => {
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
        let msg = ClientMessage::KeyInput { session_id, data, dismisses_attention: true };

        if write_message(&mut writer, &msg).await.is_err() {
            break;
        }
    }
}

async fn connect_server() -> Result<UnixStream, ScribeError> {
    let path = server_socket_path();
    info!(?path, "connecting to scribe-server");
    UnixStream::connect(&path).await.map_err(|e| ScribeError::Io { source: e })
}

async fn interactive_passthrough() -> Result<(), ScribeError> {
    let stream = connect_server().await?;
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    info!("connected");

    let workspace_id = WorkspaceId::new();
    let create_msg = ClientMessage::CreateSession {
        workspace_id,
        split_direction: None,
        cwd: None,
        size: None,
        command: None,
    };
    write_message(&mut write_half, &create_msg).await?;

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

    let output_handle = tokio::spawn(async move {
        pump_server_output(&mut read_half).await;
    });
    let stdin_handle = tokio::spawn(pump_stdin_input(session_id, write_half));

    tokio::select! {
        _ = output_handle => {},
        _ = stdin_handle => {},
    }

    Ok(())
}

async fn wait_for_windows(
    mut stream: UnixStream,
) -> Result<Vec<scribe_common::protocol::WindowInfo>, ScribeError> {
    write_message(&mut stream, &ClientMessage::ListWindows).await?;
    loop {
        let msg: ServerMessage = read_message(&mut stream).await?;
        match msg {
            ServerMessage::WindowList { windows } => return Ok(windows),
            ServerMessage::Error { message } => {
                return Err(ScribeError::ProtocolError {
                    reason: format!("server error: {message}"),
                });
            }
            other => {
                info!(?other, "ignoring unrelated server message while waiting for WindowList");
            }
        }
    }
}

fn to_automation_action(action: ActionCommand) -> AutomationAction {
    match action {
        ActionCommand::OpenSettings => AutomationAction::OpenSettings,
        ActionCommand::OpenFind => AutomationAction::OpenFind,
        ActionCommand::NewTab => AutomationAction::NewTab,
        ActionCommand::NewAiTab => AutomationAction::NewClaudeTab,
        ActionCommand::ResumeAiTab => AutomationAction::NewClaudeResumeTab,
        ActionCommand::SplitVertical => AutomationAction::SplitVertical,
        ActionCommand::SplitHorizontal => AutomationAction::SplitHorizontal,
        ActionCommand::ClosePane => AutomationAction::ClosePane,
        ActionCommand::CloseTab => AutomationAction::CloseTab,
        ActionCommand::NewWindow => AutomationAction::NewWindow,
        ActionCommand::SwitchProfile { name } => AutomationAction::SwitchProfile { name },
    }
}

async fn run_windows_command() -> Result<(), ScribeError> {
    let windows = wait_for_windows(connect_server().await?).await?;
    for window in windows {
        write_line(&format!(
            "{}\t{}\t{}",
            window.window_id.to_full_string(),
            window.session_count,
            if window.connected { "connected" } else { "detached" }
        ));
    }
    Ok(())
}

async fn run_action_command(
    window: Option<WindowId>,
    action: ActionCommand,
) -> Result<(), ScribeError> {
    let mut stream = connect_server().await?;
    let msg =
        ClientMessage::DispatchAction { window_id: window, action: to_automation_action(action) };
    write_message(&mut stream, &msg).await?;
    let response: ServerMessage = read_message(&mut stream).await?;
    parse_dispatch_response(response)?;
    Ok(())
}

fn parse_dispatch_response(msg: ServerMessage) -> Result<WindowId, ScribeError> {
    match msg {
        ServerMessage::ActionDispatched { window_id } => Ok(window_id),
        ServerMessage::Error { message } => {
            Err(ScribeError::ProtocolError { reason: format!("server error: {message}") })
        }
        other => {
            Err(ScribeError::ProtocolError { reason: format!("unexpected response: {other:?}") })
        }
    }
}

fn run_profile_command(action: ProfileCommand) -> Result<(), ScribeError> {
    match action {
        ProfileCommand::List => {
            let active = profiles::active_profile_name()?;
            for name in profiles::list_profiles()? {
                let marker = if name == active { "*" } else { " " };
                write_line(&format!("{marker} {name}"));
            }
        }
        ProfileCommand::Active => {
            write_line(&profiles::active_profile_name()?);
        }
        ProfileCommand::Save { name } => {
            let saved = profiles::save_current_as_profile(&name)?;
            write_line(&saved);
        }
        ProfileCommand::Switch { name } => {
            profiles::switch_profile(&name)?;
            write_line(&name);
        }
        ProfileCommand::Export { name, path } => {
            let exported = profiles::export_profile(&name, &path)?;
            write_line(&exported.display().to_string());
        }
        ProfileCommand::Import { name, path, activate } => {
            let imported = profiles::import_profile(&name, &path, activate)?;
            write_line(&imported);
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), ScribeError> {
    #[allow(clippy::unwrap_used, reason = "EnvFilter::new with static string cannot fail")]
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(filter).init();

    match Cli::parse().command {
        None => interactive_passthrough().await,
        Some(CliCommand::Windows) => run_windows_command().await,
        Some(CliCommand::Action { window, action }) => run_action_command(window, action).await,
        Some(CliCommand::Profile { action }) => run_profile_command(action),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_dispatch_response;
    use scribe_common::error::ScribeError;
    use scribe_common::ids::WindowId;
    use scribe_common::protocol::ServerMessage;

    #[test]
    fn dispatch_response_accepts_success_ack() {
        let window_id = WindowId::new();
        let resolved =
            parse_dispatch_response(ServerMessage::ActionDispatched { window_id }).unwrap();
        assert_eq!(resolved, window_id);
    }

    #[test]
    fn dispatch_response_returns_server_error() {
        let err = parse_dispatch_response(ServerMessage::Error {
            message: String::from("not connected"),
        })
        .unwrap_err();
        assert!(matches!(err, ScribeError::ProtocolError { .. }));
        assert!(err.to_string().contains("not connected"));
    }

    #[test]
    fn dispatch_response_rejects_unrelated_messages() {
        let err = parse_dispatch_response(ServerMessage::QuitRequested).unwrap_err();
        assert!(matches!(err, ScribeError::ProtocolError { .. }));
        assert!(err.to_string().contains("unexpected response"));
    }
}
