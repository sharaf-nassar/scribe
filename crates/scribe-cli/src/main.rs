use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};
use tokio::io::{self, AsyncRead, AsyncReadExt as _, AsyncWrite};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tracing::info;

use scribe_common::agent::{
    AgentCapability, AgentError, AgentPayload, AgentPolicyMode, AgentRequest, AgentResponse,
};
use scribe_common::config::{AgentApiConfig, load_config};
use tracing_subscriber::{EnvFilter, fmt};

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId, new_launch_id};
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
    /// One-shot local agent API commands. Data commands always emit JSON.
    Agent {
        /// Caller-supplied agent name displayed by the server.
        #[arg(long, global = true, default_value = "scribe-cli")]
        agent: String,
        /// Optional caller-supplied model name displayed alongside the agent.
        #[arg(long, global = true)]
        model: Option<String>,
        /// Request machine-readable output (agent commands are JSON by default).
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: AgentCommand,
    },
}

#[derive(Subcommand)]
enum ActionCommand {
    OpenSettings,
    OpenFind,
    NewTab,
    NewClaudeTab,
    ResumeClaudeTab,
    NewCodexTab,
    ResumeCodexTab,
    /// Compatibility alias for `new-claude-tab`.
    NewAiTab,
    /// Compatibility alias for `resume-claude-tab`.
    ResumeAiTab,
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    CloseTab,
    NewWindow,
    SwitchProfile {
        name: String,
    },
    OpenUpdateDialog,
    FocusSession {
        session_id: SessionId,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    World,
    Siblings,
    Read {
        session_id: SessionId,
        #[arg(long)]
        scrollback: Option<u32>,
    },
    Action {
        #[command(subcommand)]
        action: ActionCommand,
        #[arg(long, global = true)]
        window: Option<WindowId>,
    },
    Write {
        session_id: SessionId,
        #[arg(long)]
        text: String,
        #[arg(long)]
        submit: bool,
    },
    Capabilities,
    /// Render provider guidance from this binary's commands and current policy.
    Skill,
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
    drop(stdout.write_all(data));
    drop(stdout.flush());
}

fn write_line(line: &str) {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    write_stdout(&buf);
}

fn write_stderr_line(line: &str) {
    let mut stderr = std::io::stderr().lock();
    drop(writeln!(stderr, "{line}"));
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
            Ok(ServerMessage::SessionExited { session_id, exit_code, signal }) => {
                info!(%session_id, ?exit_code, ?signal, "session exited");
                break;
            }
            Ok(ServerMessage::CwdChanged { session_id, cwd }) => {
                info!(%session_id, ?cwd, "CWD changed");
            }
            Ok(ServerMessage::TitleChanged { session_id, title }) => {
                info!(%session_id, %title, "title changed");
            }
            Ok(ServerMessage::IconTitleChanged { session_id, title }) => {
                info!(%session_id, %title, "icon title changed");
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

/// Deadline for detecting a server that predates the agent API progress ack.
const AGENT_API_DEADLINE: Duration = Duration::from_secs(3);
/// Prompt ceiling (5 minutes) + action completion (1 minute) + transport margin.
const AGENT_API_COMPLETION_DEADLINE: Duration = Duration::from_secs(375);
const AGENT_REQUEST_ID: u64 = 1;
const MAX_AGENT_LABEL_CHARS: usize = 64;

#[derive(Debug)]
enum AgentExchangeError {
    Unreachable(ScribeError),
    Unsupported(ScribeError),
    Completion(ScribeError),
    CompletionDeadline,
}

fn agent_label(agent: &str, model: Option<String>) -> Result<String, &'static str> {
    if agent.is_empty() {
        return Err("--agent must not be empty");
    }
    if model.as_deref().is_some_and(str::is_empty) {
        return Err("--model must not be empty");
    }

    let label = model.map_or_else(|| agent.to_owned(), |model| format!("{agent} [{model}]"));
    if label.chars().count() > MAX_AGENT_LABEL_CHARS {
        return Err("the composed agent label must be at most 64 characters");
    }
    Ok(label)
}

fn origin_session_id(value: Option<OsString>) -> Result<Option<SessionId>, &'static str> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let value = value.to_str().ok_or("SCRIBE_SESSION_ID must be valid Unicode when set")?;
    SessionId::from_str(value)
        .map(Some)
        .map_err(|_| "SCRIBE_SESSION_ID must be a full valid session UUID when set")
}

fn agent_error_parts(error: &AgentError) -> (&'static str, &str) {
    match error {
        AgentError::Denied { message } => ("denied", message),
        AgentError::PromptTimeout { message } => ("prompt_timeout", message),
        AgentError::NotFound { message } => ("not_found", message),
        AgentError::AmbiguousTarget { message } => ("ambiguous_target", message),
        AgentError::Unsupported { message } => ("unsupported", message),
        AgentError::TooLarge { message } => ("too_large", message),
        AgentError::Busy { message } => ("busy", message),
        AgentError::VersionMismatch { message } => ("version_mismatch", message),
        AgentError::ActionFailed { message } => ("action_failed", message),
        AgentError::Internal { message } => ("internal", message),
    }
}

fn agent_failure_envelope(code: &str, message: &str) -> Result<String, serde_json::Error> {
    let code = serde_json::to_string(code)?;
    let message = serde_json::to_string(message)?;
    Ok(format!(r#"{{"v":1,"ok":false,"error":{{"code":{code},"message":{message}}}}}"#))
}

fn agent_envelope(result: Result<AgentPayload, AgentError>) -> Result<String, serde_json::Error> {
    match result {
        Ok(data) => Ok(format!(r#"{{"v":1,"ok":true,"data":{}}}"#, serde_json::to_string(&data)?)),
        Err(error) => {
            let (code, message) = agent_error_parts(&error);
            agent_failure_envelope(code, message)
        }
    }
}

fn print_agent_envelope(envelope: Result<String, serde_json::Error>) -> bool {
    match envelope {
        Ok(envelope) => {
            write_line(&envelope);
            true
        }
        Err(error) => {
            write_stderr_line(&format!("failed to serialize agent response: {error}"));
            write_line(
                r#"{"v":1,"ok":false,"error":{"code":"internal","message":"Failed to serialize agent response."}}"#,
            );
            false
        }
    }
}

fn print_agent_failure(code: &str, message: &str) -> bool {
    print_agent_envelope(agent_failure_envelope(code, message))
}

fn collect_agent_command_paths(
    command: &clap::Command,
    path: &mut Vec<String>,
    paths: &mut Vec<Vec<String>>,
) {
    for subcommand in command.get_subcommands() {
        path.push(subcommand.get_name().into());
        paths.push(path.clone());
        collect_agent_command_paths(subcommand, path, paths);
        _ = path.pop();
    }
}

fn agent_command_paths() -> Vec<Vec<String>> {
    let command = Cli::command();
    let mut paths = Vec::new();
    for agent in command.get_subcommands().filter(|subcommand| subcommand.get_name() == "agent") {
        collect_agent_command_paths(agent, &mut Vec::new(), &mut paths);
    }
    paths
}

fn agent_command_capability(path: &[String]) -> Option<AgentCapability> {
    match path {
        [command] if matches!(command.as_str(), "world" | "siblings" | "capabilities") => {
            Some(AgentCapability::ReadMetadata)
        }
        [command] if command == "read" => Some(AgentCapability::ReadContent),
        [command] if command == "write" => Some(AgentCapability::WriteInput),
        [category, action] if category == "action" => match action.as_str() {
            "close-pane" | "close-tab" | "open-update-dialog" => {
                Some(AgentCapability::DispatchDestructiveAction)
            }
            "open-settings" | "open-find" | "new-tab" | "new-claude-tab" | "resume-claude-tab"
            | "new-codex-tab" | "resume-codex-tab" | "new-ai-tab" | "resume-ai-tab"
            | "split-vertical" | "split-horizontal" | "new-window" | "switch-profile"
            | "focus-session" => Some(AgentCapability::DispatchAction),
            _ => None,
        },
        _ => None,
    }
}

fn agent_policy_mode(policy: &AgentApiConfig, capability: AgentCapability) -> AgentPolicyMode {
    match capability {
        AgentCapability::ReadMetadata => policy.read_metadata,
        AgentCapability::ReadContent => policy.read_content,
        AgentCapability::DispatchAction => policy.dispatch_action,
        AgentCapability::DispatchDestructiveAction => policy.dispatch_destructive_action,
        AgentCapability::WriteInput => policy.write_input,
    }
}

const fn agent_capability_settings_path(capability: AgentCapability) -> &'static str {
    match capability {
        AgentCapability::ReadMetadata => "agent_api.read_metadata",
        AgentCapability::ReadContent => "agent_api.read_content",
        AgentCapability::DispatchAction => "agent_api.dispatch_action",
        AgentCapability::DispatchDestructiveAction => "agent_api.dispatch_destructive_action",
        AgentCapability::WriteInput => "agent_api.write_input",
    }
}

fn append_agent_command_policy(markdown: &mut String, policy: &AgentApiConfig, path: &[String]) {
    let Some(capability) = agent_command_capability(path) else {
        match path {
            [command] if command == "skill" => markdown.push_str("renders this guidance."),
            [command] if command == "action" => {
                markdown.push_str("choose a subcommand below.");
            }
            _ => {
                markdown.push_str("unavailable; no capability policy is defined for this command.");
            }
        }
        return;
    };

    let settings_path = agent_capability_settings_path(capability);
    match agent_policy_mode(policy, capability) {
        AgentPolicyMode::Deny => {
            markdown.push_str("unavailable; enable `");
            markdown.push_str(settings_path);
            markdown.push_str("` in Settings > Agent API.");
        }
        AgentPolicyMode::Prompt => {
            markdown.push_str("available after confirmation (`");
            markdown.push_str(settings_path);
            markdown.push_str(" = prompt`).");
        }
        AgentPolicyMode::Allow => {
            markdown.push_str("available (`");
            markdown.push_str(settings_path);
            markdown.push_str(" = allow`).");
        }
    }
}

fn render_agent_skill(policy: &AgentApiConfig) -> String {
    let mut markdown = String::from(
        "# Scribe agent control\n\n\
         Use these commands only from a Scribe pane. If `SCRIBE_SESSION_ID` is unset, no-op: do not run `scribe agent` commands.\n\n\
         Data commands write versioned JSON to stdout. Use `scribe agent <command> --help` for arguments.\n\n\
         Set `--agent NAME` to identify the caller and optionally `--model MODEL`; the server displays `NAME [MODEL]` and the composed label is limited to 64 characters.\n\n\
         ## Commands\n\n",
    );
    for path in agent_command_paths() {
        markdown.push_str("- `scribe agent ");
        markdown.push_str(&path.join(" "));
        markdown.push_str("` — ");
        append_agent_command_policy(&mut markdown, policy, &path);
        markdown.push('\n');
    }
    markdown
}

fn run_agent_skill_command() -> ExitCode {
    match load_config() {
        Ok(config) => {
            write_stdout(render_agent_skill(&config.agent_api).as_bytes());
            ExitCode::SUCCESS
        }
        Err(error) => {
            write_stderr_line(&format!("failed to load agent API configuration: {error}"));
            ExitCode::from(1)
        }
    }
}

fn build_agent_request(
    command: AgentCommand,
    agent_label: String,
    origin_session_id: Option<SessionId>,
) -> Option<AgentRequest> {
    Some(match command {
        AgentCommand::World => AgentRequest::World {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
        },
        AgentCommand::Siblings => AgentRequest::Siblings {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
        },
        AgentCommand::Read { session_id, scrollback } => AgentRequest::ReadScreen {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
            session_id,
            scrollback_lines: scrollback,
        },
        AgentCommand::Action { action, window } => AgentRequest::DispatchAction {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
            action: to_automation_action(action),
            window,
        },
        AgentCommand::Write { session_id, text, submit } => AgentRequest::WriteInput {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
            session_id,
            text,
            submit,
        },
        AgentCommand::Capabilities => AgentRequest::Capabilities {
            request_id: AGENT_REQUEST_ID,
            agent_label,
            origin_session_id,
            progress_ack: true,
        },
        AgentCommand::Skill => return None,
    })
}

async fn exchange_agent_request(
    request: &AgentRequest,
) -> Result<AgentResponse, AgentExchangeError> {
    let mut stream = UnixStream::connect(server_socket_path())
        .await
        .map_err(|source| AgentExchangeError::Unreachable(ScribeError::Io { source }))?;
    exchange_agent_request_on(&mut stream, request).await
}

async fn exchange_agent_request_on<S>(
    stream: &mut S,
    request: &AgentRequest,
) -> Result<AgentResponse, AgentExchangeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_message(stream, &ClientMessage::AgentRequest(request.clone()))
        .await
        .map_err(AgentExchangeError::Unsupported)?;
    let first: ServerMessage = timeout(AGENT_API_DEADLINE, read_message(stream))
        .await
        .map_err(|_| {
            AgentExchangeError::Unsupported(ScribeError::ProtocolError {
                reason: String::from("server did not answer the agent API handshake"),
            })
        })?
        .map_err(AgentExchangeError::Unsupported)?;
    let response = match first {
        ServerMessage::AgentResponse(response) => response,
        ServerMessage::AgentRequestAccepted { request_id }
            if request_id == request.request_id() =>
        {
            let response: ServerMessage =
                timeout(AGENT_API_COMPLETION_DEADLINE, read_message(stream))
                    .await
                    .map_err(|_| AgentExchangeError::CompletionDeadline)?
                    .map_err(AgentExchangeError::Completion)?;
            let ServerMessage::AgentResponse(response) = response else {
                return Err(AgentExchangeError::Completion(ScribeError::ProtocolError {
                    reason: String::from("server did not complete the acknowledged agent request"),
                }));
            };
            response
        }
        _ => {
            return Err(AgentExchangeError::Unsupported(ScribeError::ProtocolError {
                reason: String::from("server did not return an agent response"),
            }));
        }
    };
    Ok(response)
}

fn agent_response_exit(response: AgentResponse) -> ExitCode {
    if response.request_id != AGENT_REQUEST_ID {
        _ = print_agent_failure("internal", "Scribe server returned a mismatched agent response.");
        return ExitCode::from(1);
    }

    let exit_code = match &response.result {
        Ok(_) => ExitCode::SUCCESS,
        Err(AgentError::Unsupported { .. }) => ExitCode::from(3),
        Err(_) => ExitCode::from(1),
    };
    if print_agent_envelope(agent_envelope(response.result)) {
        exit_code
    } else {
        ExitCode::from(1)
    }
}

async fn run_agent_command(
    agent: String,
    model: Option<String>,
    command: AgentCommand,
) -> ExitCode {
    if matches!(&command, AgentCommand::Skill) {
        return run_agent_skill_command();
    }

    let agent_label = match agent_label(&agent, model) {
        Ok(label) => label,
        Err(message) => {
            _ = print_agent_failure("usage", message);
            return ExitCode::from(2);
        }
    };
    let origin_session_id = match origin_session_id(std::env::var_os("SCRIBE_SESSION_ID")) {
        Ok(session_id) => session_id,
        Err(message) => {
            _ = print_agent_failure("usage", message);
            return ExitCode::from(2);
        }
    };
    let Some(request) = build_agent_request(command, agent_label, origin_session_id) else {
        return run_agent_skill_command();
    };

    match exchange_agent_request(&request).await {
        Ok(response) => agent_response_exit(response),
        Err(AgentExchangeError::Unreachable(error)) => {
            write_stderr_line(&format!("agent API server is unreachable: {error}"));
            _ = print_agent_failure("unreachable", "Scribe server is unreachable.");
            ExitCode::from(3)
        }
        Err(AgentExchangeError::Unsupported(error)) => {
            write_stderr_line(&format!("agent API is unsupported: {error}"));
            _ = print_agent_failure(
                "unsupported",
                "The connected Scribe server does not support the agent API.",
            );
            ExitCode::from(3)
        }
        Err(AgentExchangeError::Completion(error)) => {
            write_stderr_line(&format!(
                "agent API exchange failed after request acknowledgement: {error}"
            ));
            _ = print_agent_failure(
                "internal",
                "Scribe server disconnected while processing the agent request.",
            );
            ExitCode::from(1)
        }
        Err(AgentExchangeError::CompletionDeadline) => {
            write_stderr_line(
                "agent API completion deadline elapsed after request acknowledgement",
            );
            _ = print_agent_failure(
                "internal",
                "Scribe server did not complete the acknowledged agent request.",
            );
            ExitCode::from(1)
        }
    }
}

async fn interactive_passthrough() -> Result<(), ScribeError> {
    let stream = connect_server().await?;
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    info!("connected");

    let workspace_id = WorkspaceId::new();
    // A CLI session mints its own launch id for the same reason the GUI does:
    // without one the server has nothing to name the session's env envelope
    // after, and env persistence never starts for the session.
    let create_msg = ClientMessage::CreateSession {
        workspace_id,
        split_direction: None,
        cwd: None,
        size: None,
        command: None,
        ai_launch: None,
        shell_tool: None,
        env_envelope_id: Some(new_launch_id()),
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
        ActionCommand::NewClaudeTab | ActionCommand::NewAiTab => AutomationAction::NewClaudeTab,
        ActionCommand::ResumeClaudeTab | ActionCommand::ResumeAiTab => {
            AutomationAction::NewClaudeResumeTab
        }
        ActionCommand::NewCodexTab => AutomationAction::NewCodexTab,
        ActionCommand::ResumeCodexTab => AutomationAction::NewCodexResumeTab,
        ActionCommand::SplitVertical => AutomationAction::SplitVertical,
        ActionCommand::SplitHorizontal => AutomationAction::SplitHorizontal,
        ActionCommand::ClosePane => AutomationAction::ClosePane,
        ActionCommand::CloseTab => AutomationAction::CloseTab,
        ActionCommand::NewWindow => AutomationAction::NewWindow,
        ActionCommand::SwitchProfile { name } => AutomationAction::SwitchProfile { name },
        ActionCommand::OpenUpdateDialog => AutomationAction::OpenUpdateDialog,
        ActionCommand::FocusSession { session_id } => AutomationAction::FocusSession { session_id },
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
    let resolved_action = to_automation_action(action);
    let msg = ClientMessage::DispatchAction { window_id: window, action: resolved_action };
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
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().map_or(EnvFilter::new("info"), |filter| filter);
    // Diagnostics go to stderr: stdout is reserved for data (agent commands
    // emit versioned JSON there, and `agent skill` output is installed
    // verbatim into skill files).
    fmt().with_env_filter(filter).with_writer(std::io::stderr).init();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code =
                if error.exit_code() == 0 { ExitCode::SUCCESS } else { ExitCode::from(2) };
            _ = error.print();
            return exit_code;
        }
    };
    let result = match cli.command {
        None => interactive_passthrough().await,
        Some(CliCommand::Windows) => run_windows_command().await,
        Some(CliCommand::Action { window, action }) => run_action_command(window, action).await,
        Some(CliCommand::Profile { action }) => run_profile_command(action),
        Some(CliCommand::Agent { agent, model, json: _, command }) => {
            return run_agent_command(agent, model, command).await;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_stderr_line(&format!("Error: {error}"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{
        agent_envelope, agent_label, exchange_agent_request_on, origin_session_id,
        parse_dispatch_response, render_agent_skill,
    };
    use clap::{Command, CommandFactory, Parser};
    use scribe_common::agent::{AgentError, AgentPayload, AgentRequest, AgentResponse};
    use scribe_common::config::AgentApiConfig;
    use scribe_common::error::ScribeError;
    use scribe_common::framing::{read_message, write_message};
    use scribe_common::ids::WindowId;
    use scribe_common::protocol::{ClientMessage, ServerMessage};

    use crate::Cli;

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

    fn assert_agent_skill_covers_tree(command: &Command, path: &mut Vec<String>, skill: &str) {
        for subcommand in command.get_subcommands() {
            path.push(subcommand.get_name().into());
            let rendered = format!("`scribe agent {}`", path.join(" "));
            assert!(skill.contains(&rendered), "missing {rendered}");
            assert_agent_skill_covers_tree(subcommand, path, skill);
            _ = path.pop();
        }
    }

    #[test]
    fn agent_skill_names_every_clap_subcommand() {
        let skill = render_agent_skill(&AgentApiConfig::default());
        for agent in
            Cli::command().get_subcommands().filter(|command| command.get_name() == "agent")
        {
            assert_agent_skill_covers_tree(agent, &mut Vec::new(), &skill);
        }
    }

    #[test]
    fn agent_skill_instructs_noop_and_marks_denied_capabilities_unavailable() {
        let skill = render_agent_skill(&AgentApiConfig::default());
        assert!(skill.contains("If `SCRIBE_SESSION_ID` is unset, no-op"));
        for settings_path in [
            "agent_api.read_metadata",
            "agent_api.read_content",
            "agent_api.dispatch_action",
            "agent_api.dispatch_destructive_action",
            "agent_api.write_input",
        ] {
            assert!(skill.contains(&format!("unavailable; enable `{settings_path}`")));
        }
    }

    #[test]
    fn agent_skill_reflects_allowed_and_prompted_policy() {
        let policy = AgentApiConfig {
            read_content: scribe_common::agent::AgentPolicyMode::Allow,
            write_input: scribe_common::agent::AgentPolicyMode::Prompt,
            ..AgentApiConfig::default()
        };
        let skill = render_agent_skill(&policy);

        assert!(
            skill.contains("`scribe agent read` — available (`agent_api.read_content = allow`).")
        );
        assert!(skill.contains(
            "`scribe agent write` — available after confirmation (`agent_api.write_input = prompt`)."
        ));
    }

    #[test]
    fn agent_command_tree_accepts_every_v1_command() {
        let session_id = scribe_common::ids::SessionId::new().to_full_string();
        let window_id = WindowId::new().to_full_string();
        let commands = [
            vec![
                String::from("scribe"),
                String::from("agent"),
                String::from("world"),
                String::from("--agent"),
                String::from("runner"),
                String::from("--model"),
                String::from("model-x"),
            ],
            vec![String::from("scribe"), String::from("agent"), String::from("siblings")],
            vec![
                String::from("scribe"),
                String::from("agent"),
                String::from("read"),
                session_id.clone(),
                String::from("--scrollback"),
                String::from("10"),
            ],
            vec![
                String::from("scribe"),
                String::from("agent"),
                String::from("action"),
                String::from("new-tab"),
                String::from("--window"),
                window_id,
                String::from("--json"),
            ],
            vec![
                String::from("scribe"),
                String::from("agent"),
                String::from("write"),
                session_id,
                String::from("--text"),
                String::from("echo ok"),
                String::from("--submit"),
            ],
            vec![String::from("scribe"), String::from("agent"), String::from("capabilities")],
            vec![String::from("scribe"), String::from("agent"), String::from("skill")],
        ];
        for command in commands {
            assert!(Cli::try_parse_from(command).is_ok());
        }
    }

    #[test]
    fn agent_label_composes_agent_and_model_within_the_wire_limit() {
        assert_eq!(
            agent_label("runner", Some(String::from("model-x"))).unwrap(),
            "runner [model-x]"
        );
        assert!(agent_label("", None).is_err());
        assert!(agent_label(&"x".repeat(65), None).is_err());
    }

    #[test]
    fn origin_session_id_requires_a_valid_full_uuid_when_set() {
        let session_id = scribe_common::ids::SessionId::new();
        let parsed = origin_session_id(Some(session_id.to_full_string().into())).unwrap();
        assert_eq!(parsed, Some(session_id));
        assert!(origin_session_id(Some(String::from("not-a-session").into())).is_err());
        assert_eq!(origin_session_id(Some(OsString::new())).unwrap(), None);
        assert_eq!(origin_session_id(None).unwrap(), None);
    }

    #[tokio::test]
    async fn agent_exchange_uses_framed_one_shot_request_and_response() {
        let session_id = scribe_common::ids::SessionId::new();
        let request = AgentRequest::World {
            request_id: 1,
            agent_label: String::from("runner [model-x]"),
            origin_session_id: Some(session_id),
            progress_ack: true,
        };
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let message: ClientMessage = read_message(&mut server).await.unwrap();
            assert!(matches!(
                message,
                ClientMessage::AgentRequest(AgentRequest::World {
                    request_id: 1,
                    agent_label,
                    origin_session_id: Some(origin),
                    progress_ack: true,
                }) if agent_label == "runner [model-x]" && origin == session_id
            ));
            write_message(
                &mut server,
                &ServerMessage::AgentResponse(AgentResponse {
                    request_id: 1,
                    result: Ok(AgentPayload::WriteInput),
                }),
            )
            .await
            .unwrap();
        });

        let response = exchange_agent_request_on(&mut client, &request).await.unwrap();
        assert!(matches!(response.result, Ok(AgentPayload::WriteInput)));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn agent_exchange_survives_a_prompt_parked_reply() {
        let request = AgentRequest::World {
            request_id: 1,
            agent_label: String::from("runner"),
            origin_session_id: None,
            progress_ack: true,
        };
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let _: ClientMessage = read_message(&mut server).await.unwrap();
            write_message(&mut server, &ServerMessage::AgentRequestAccepted { request_id: 1 })
                .await
                .unwrap();
            tokio::time::sleep(super::AGENT_API_DEADLINE + std::time::Duration::from_secs(1)).await;
            write_message(
                &mut server,
                &ServerMessage::AgentResponse(AgentResponse {
                    request_id: 1,
                    result: Ok(AgentPayload::WriteInput),
                }),
            )
            .await
            .unwrap();
        });

        let response = exchange_agent_request_on(&mut client, &request).await.unwrap();
        assert!(matches!(response.result, Ok(AgentPayload::WriteInput)));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn agent_exchange_detects_a_silent_old_server_within_the_handshake_deadline() {
        let request = AgentRequest::World {
            request_id: 1,
            agent_label: String::from("runner"),
            origin_session_id: None,
            progress_ack: true,
        };
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            let _: ClientMessage = read_message(&mut server).await.unwrap();
            std::future::pending::<()>().await;
        });

        let started = std::time::Instant::now();
        let result = exchange_agent_request_on(&mut client, &request).await;
        assert!(matches!(result, Err(super::AgentExchangeError::Unsupported(_))));
        assert!(
            started.elapsed() >= super::AGENT_API_DEADLINE
                && started.elapsed()
                    < super::AGENT_API_DEADLINE + std::time::Duration::from_secs(1),
            "silent old-server detection did not use the handshake deadline"
        );
        server_task.abort();
    }

    #[test]
    fn agent_envelopes_are_versioned_json() {
        let success: serde_json::Value =
            serde_json::from_str(&agent_envelope(Ok(AgentPayload::WriteInput)).unwrap()).unwrap();
        assert_eq!(
            success,
            serde_json::json!({"v": 1, "ok": true, "data": {"type": "write_input"}})
        );

        let error: serde_json::Value = serde_json::from_str(
            &agent_envelope(Err(AgentError::Denied { message: String::from("denied") })).unwrap(),
        )
        .unwrap();
        assert_eq!(
            error,
            serde_json::json!({"v": 1, "ok": false, "error": {"code": "denied", "message": "denied"}})
        );
    }
}
