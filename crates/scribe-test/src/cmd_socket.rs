use std::path::PathBuf;

use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::SessionId;
use scribe_common::screen::ScreenSnapshot;
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;

// ---------------------------------------------------------------------------
// Request / Response protocol
// ---------------------------------------------------------------------------

/// Request from a CLI subcommand to the daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    CreateSession,
    AttachSession {
        session_id: SessionId,
    },
    CloseSession {
        session_id: SessionId,
    },
    Send {
        session_id: SessionId,
        data: Vec<u8>,
    },
    Resize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    RequestScreenshot {
        session_id: SessionId,
    },
    RequestSnapshot {
        session_id: SessionId,
    },
    WaitOutput {
        session_id: SessionId,
        pattern: String,
        timeout_ms: u64,
    },
    WaitCwd {
        session_id: SessionId,
        path: String,
        timeout_ms: u64,
    },
    WaitIdle {
        session_id: SessionId,
        quiet_ms: u64,
        timeout_ms: u64,
    },
    AssertCell {
        session_id: SessionId,
        row: u16,
        col: u16,
        expected: char,
    },
    AssertCursor {
        session_id: SessionId,
        row: u16,
        col: u16,
    },
    AssertExit {
        session_id: SessionId,
        expected_code: i32,
        timeout_ms: u64,
    },
    /// Compare the current screen against a reference snapshot (cell content,
    /// cursor position, cursor visibility).
    AssertSnapshotMatch {
        session_id: SessionId,
        reference: Box<ScreenSnapshot>,
    },
    Shutdown,
}

/// Response from the daemon to a CLI subcommand.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    Ok,
    SessionCreated { session_id: SessionId },
    ScreenshotData { snapshot: Box<ScreenSnapshot> },
    AssertFailed { message: String },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Returns the Unix-domain socket path for the test daemon.
///
/// Path: `/run/user/{uid}/scribe/test-daemon.sock`
pub fn daemon_socket_path() -> PathBuf {
    let uid = nix::unistd::geteuid();
    PathBuf::from(format!("/run/user/{uid}/scribe/test-daemon.sock"))
}

// ---------------------------------------------------------------------------
// One-shot request helper
// ---------------------------------------------------------------------------

/// Connect to the test daemon, send one request, receive one response.
///
/// Creates a short-lived tokio runtime internally so callers do not need an
/// async context.
///
/// # Errors
///
/// Returns `ScribeError::Io` if the connection or runtime creation fails,
/// `ScribeError::Serialization` / `ScribeError::Deserialization` on codec
/// errors, or `ScribeError::ProtocolError` if a framing limit is hit.
pub fn send_request(request: &DaemonRequest) -> Result<DaemonResponse, ScribeError> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| ScribeError::Io { source: e })?;
    rt.block_on(send_request_async(request))
}

/// Async implementation of the one-shot request/response exchange.
async fn send_request_async(request: &DaemonRequest) -> Result<DaemonResponse, ScribeError> {
    let path = daemon_socket_path();
    let stream = UnixStream::connect(&path).await.map_err(|e| ScribeError::Io { source: e })?;

    let (mut reader, mut writer) = stream.into_split();

    write_message(&mut writer, request).await?;
    read_message(&mut reader).await
}
