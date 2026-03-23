use std::str::FromStr as _;

use scribe_common::ids::SessionId;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Wait until output matching a regex pattern appears in the session.
pub fn wait_output(session_id: &str, pattern: &str, timeout_ms: u64) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request =
        DaemonRequest::WaitOutput { session_id: id, pattern: pattern.to_owned(), timeout_ms };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Wait until the session's CWD matches the given path.
pub fn wait_cwd(session_id: &str, path: &str, timeout_ms: u64) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request = DaemonRequest::WaitCwd { session_id: id, path: path.to_owned(), timeout_ms };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Wait until the session has been idle for at least `quiet_ms` milliseconds.
pub fn wait_idle(session_id: &str, quiet_ms: u64, timeout_ms: u64) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request = DaemonRequest::WaitIdle { session_id: id, quiet_ms, timeout_ms };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}
