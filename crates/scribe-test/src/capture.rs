use std::io::{self, Write as _};
use std::path::Path;
use std::str::FromStr as _;

use scribe_common::ids::SessionId;
use scribe_common::protocol::{BeadsIssueWrite, BeadsIssueWriteGuards};

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

/// Print the AI chrome a client would draw for a session.
///
/// Emits one `prompt-bar: <meter>` line whenever the session has a context
/// percentage, and one `tab: <suffix>` line only when the percentage has reached
/// the warn band. That makes the two lines directly greppable by the functional
/// E2E scripts: a percentage below warn appears exactly once (prompt bar only),
/// at or above warn it appears twice (prompt bar plus tab).
pub fn ai_chrome(session_id: &str) -> Result<(), TestError> {
    let id = SessionId::from_str(session_id)
        .map_err(|e| TestError::InfraError(format!("invalid session id: {e}")))?;

    let response = send_request(&DaemonRequest::RequestAiChrome { session_id: id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::AiChrome { prompt_bar, tab } => {
            if let Some(meter) = prompt_bar {
                writeln!(io::stdout(), "prompt-bar: {meter}")
                    .map_err(|e| TestError::InfraError(format!("failed to write chrome: {e}")))?;
            }
            if let Some(suffix) = tab {
                writeln!(io::stdout(), "tab:{suffix}")
                    .map_err(|e| TestError::InfraError(format!("failed to write chrome: {e}")))?;
            }
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Refresh the current workspace's Beads board and print its terminal state.
pub fn beads_board() -> Result<(), TestError> {
    let response = send_request(&DaemonRequest::RequestBeadsBoard)
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::BeadsBoard { state } => {
            serde_json::to_writer(io::stdout().lock(), &state)
                .map_err(|e| TestError::InfraError(format!("failed to serialize board: {e}")))?;
            writeln!(io::stdout())
                .map_err(|e| TestError::InfraError(format!("failed to write board: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Assemble one epic's Flow graph through the real server and print its outcome.
pub fn beads_epic_graph(epic_id: String) -> Result<(), TestError> {
    let response = send_request(&DaemonRequest::RequestBeadsEpicGraph { epic_id })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        DaemonResponse::BeadsEpicGraph { outcome, .. } => {
            serde_json::to_writer(io::stdout().lock(), &outcome)
                .map_err(|e| TestError::InfraError(format!("failed to serialize graph: {e}")))?;
            writeln!(io::stdout())
                .map_err(|e| TestError::InfraError(format!("failed to write graph: {e}")))?;
            Ok(())
        }
        DaemonResponse::Error { message } => Err(TestError::InfraError(message)),
        other => Err(TestError::InfraError(format!("unexpected response: {other:?}"))),
    }
}

/// Execute a typed issue write and print its result plus push observation.
pub fn beads_write(
    issue_id: String,
    verb: BeadsIssueWrite,
    guards: BeadsIssueWriteGuards,
) -> Result<(), TestError> {
    let response = send_request(&DaemonRequest::BeadsIssueWrite { issue_id, verb, guards })
        .map_err(|e| TestError::InfraError(e.to_string()))?;

    match response {
        response @ DaemonResponse::BeadsIssueWrite { .. } => {
            serde_json::to_writer(io::stdout().lock(), &response).map_err(|e| {
                TestError::InfraError(format!("failed to serialize Beads write: {e}"))
            })?;
            writeln!(io::stdout())
                .map_err(|e| TestError::InfraError(format!("failed to write Beads result: {e}")))?;
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
