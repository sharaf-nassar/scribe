use std::io::{self, Write as _};
use std::path::PathBuf;
use std::str::FromStr as _;

use scribe_common::ai_state::AiProvider;
use scribe_common::ids::SessionId;
use scribe_common::protocol::AiResumeMode;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Optional launch details accepted by `session create`.
pub struct CreateOptions {
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub ai_provider: Option<AiProvider>,
    pub ai_resume_mode: Option<AiResumeMode>,
    pub ai_conversation_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env_envelope_id: Option<String>,
}

/// Create a new terminal session via the daemon.
///
/// Sends `CreateSession` and prints the resulting session UUID to stdout.
/// `cols`/`rows` name the grid the PTY is spawned at, the way a real client
/// names the pane the session is about to fill; omitting them keeps the
/// server's 80x24 default. The AI launch fields remain separate so the daemon
/// exercises the same `AiLaunchSpec` construction as the production client
/// path.
///
/// # Errors
///
/// Returns [`TestError::InfraError`] when the daemon is unreachable or the
/// server refused the create.
pub fn create(options: CreateOptions) -> Result<(), TestError> {
    let request = DaemonRequest::CreateSession {
        cols: options.cols,
        rows: options.rows,
        ai_provider: options.ai_provider,
        ai_resume_mode: options.ai_resume_mode,
        ai_conversation_id: options.ai_conversation_id,
        cwd: options.cwd,
        env_envelope_id: options.env_envelope_id,
    };
    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::SessionCreated { session_id } => {
            writeln!(io::stdout(), "{}", session_id.to_full_string())
                .map_err(|e| TestError::InfraError(format!("failed to write session id: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Print the window UUID the server assigned the daemon.
///
/// The visual E2E rig exports it as `SCRIBE_JOIN_WINDOW` before launching the
/// GPUI client, so the client joins the daemon's window share and renders the
/// very panes the daemon's `wait-output` / `snapshot` assertions drive, instead
/// of being handed a fresh empty window of its own.
///
/// # Errors
///
/// Returns [`TestError::InfraError`] when the daemon is unreachable or has not
/// yet received its `Welcome`.
pub fn print_window_id() -> Result<(), TestError> {
    let response =
        send_request(&DaemonRequest::WindowId).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::WindowId { window_id } => {
            writeln!(io::stdout(), "{}", window_id.to_full_string())
                .map_err(|e| TestError::InfraError(format!("failed to write window id: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Print the launch (env-envelope) id the daemon minted for a session it
/// created.
///
/// An E2E script uses it to assert that the harness create path names an
/// envelope — a `CreateSession` without one can never persist its environment,
/// which is what made env persistence unobservable from the harness.
///
/// # Errors
///
/// Returns [`TestError::InfraError`] when the daemon is unreachable or did not
/// create the session itself (an attach carries no launch id of its own).
pub fn print_envelope_id(session_id: &str) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::EnvelopeId { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::EnvelopeId { envelope_id } => {
            writeln!(io::stdout(), "{envelope_id}")
                .map_err(|e| TestError::InfraError(format!("failed to write envelope id: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Attach to an existing (detached) session on the server.
///
/// Sends `AttachSession` and prints the confirmed session UUID to stdout.
/// `cols`/`rows` name the grid to attach at, which is what drives the attach
/// flow's pre-snapshot resize; omitting them sends no dimensions and leaves the
/// session's geometry alone.
///
/// # Errors
///
/// Returns [`TestError::InfraError`] when the session id is malformed, the
/// daemon is unreachable, or the server denied the attach.
pub fn attach(session_id: &str, cols: Option<u16>, rows: Option<u16>) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::AttachSession { session_id: id, cols, rows })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::SessionCreated { session_id: confirmed } => {
            writeln!(io::stdout(), "{}", confirmed.to_full_string())
                .map_err(|e| TestError::InfraError(format!("failed to write session id: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Close an existing terminal session.
pub fn close(session_id: &str) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::CloseSession { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}
