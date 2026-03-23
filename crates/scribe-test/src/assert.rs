use std::str::FromStr as _;

use scribe_common::ids::SessionId;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Assert that a specific cell contains the expected character.
pub fn assert_cell(session_id: &str, row: u16, col: u16, expected: char) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request = DaemonRequest::AssertCell { session_id: id, row, col, expected };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::AssertFailed { message } => Err(TestError::TestFailure(message)),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Assert that the cursor is at the expected position.
pub fn assert_cursor(session_id: &str, row: u16, col: u16) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request = DaemonRequest::AssertCursor { session_id: id, row, col };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::AssertFailed { message } => Err(TestError::TestFailure(message)),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Assert that a session exits with the expected exit code.
pub fn assert_exit(session_id: &str, code: i32, timeout_ms: u64) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let request = DaemonRequest::AssertExit { session_id: id, expected_code: code, timeout_ms };

    let response = send_request(&request).map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::Ok => Ok(()),
        DaemonResponse::AssertFailed { message } => Err(TestError::TestFailure(message)),
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}
