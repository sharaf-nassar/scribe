use std::io::{self, Write as _};
use std::str::FromStr as _;

use scribe_common::ids::SessionId;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Create a new terminal session via the daemon.
///
/// Sends `CreateSession` and prints the resulting session UUID to stdout.
pub fn create() -> Result<(), TestError> {
    let response = send_request(&DaemonRequest::CreateSession)
        .map_err(|e| TestError::InfraError(e.to_string()))?;

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

/// Attach to an existing (detached) session on the server.
///
/// Sends `AttachSession` and prints the confirmed session UUID to stdout.
pub fn attach(session_id: &str) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::AttachSession { session_id: id })
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
