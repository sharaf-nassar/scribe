use std::path::Path;
use std::str::FromStr as _;

use scribe_common::ids::SessionId;

use crate::TestError;
use crate::cmd_socket::{DaemonRequest, DaemonResponse, send_request};

/// Capture a screenshot of a session and render it to a PNG file.
pub fn screenshot(session_id: &str, path: &Path) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::RequestScreenshot { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::ScreenshotData { snapshot } => {
            crate::render::render_to_png(&snapshot, path)
                .map_err(|e| TestError::InfraError(format!("render failed: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Capture a text snapshot of a session and save it as JSON.
pub fn snapshot(session_id: &str, path: &Path) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::RequestSnapshot { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::ScreenshotData { snapshot } => {
            let json = serde_json::to_string_pretty(&*snapshot)
                .map_err(|e| TestError::InfraError(format!("failed to serialize snapshot: {e}")))?;
            std::fs::write(path, json)
                .map_err(|e| TestError::InfraError(format!("failed to write file: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}
