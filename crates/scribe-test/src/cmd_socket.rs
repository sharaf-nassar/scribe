use std::path::PathBuf;

use scribe_common::ai_state::AiProvider;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId};
use scribe_common::protocol::{AiResumeMode, AutomationAction, BeadsBoardState};
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
    /// Create a session, optionally naming the grid the PTY is spawned at.
    ///
    /// A real client always names one — the pane the session is about to be
    /// rendered in — so the PTY never starts on a placeholder grid it has to be
    /// resized off. `None` keeps the server's own 80x24 default, which is what
    /// every pre-existing E2E script was written against.
    CreateSession {
        cols: Option<u16>,
        rows: Option<u16>,
        #[serde(default)]
        ai_provider: Option<AiProvider>,
        #[serde(default)]
        ai_resume_mode: Option<AiResumeMode>,
        #[serde(default)]
        ai_conversation_id: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        env_envelope_id: Option<String>,
    },
    /// Attach to a session, optionally naming the grid to attach at.
    ///
    /// `None` attaches with no dimensions at all, which leaves the session's
    /// geometry untouched; naming the grid exercises the attach flow's
    /// pre-snapshot resize the way a real client's tab switch does.
    AttachSession {
        session_id: SessionId,
        cols: Option<u16>,
        rows: Option<u16>,
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
    /// Spec 017 US1-2: assert a session's child died on a specific signal,
    /// which `AssertExit` cannot express — the wire keeps the terminating
    /// signal in its own field rather than folding it into `exit_code`.
    AssertSignal {
        session_id: SessionId,
        expected_signal: i32,
        timeout_ms: u64,
    },
    /// Spec 017 US6-4: assert no zero-byte `PtyOutput` frame ever arrived for a
    /// session. Filters that swallow a whole PTY chunk must drop the frame
    /// instead of shipping an empty one the whole pipeline still pays for.
    AssertNoEmptyOutput {
        session_id: SessionId,
    },
    /// Compare the current screen against a reference snapshot (cell content,
    /// cursor position, cursor visibility).
    AssertSnapshotMatch {
        session_id: SessionId,
        reference: Box<ScreenSnapshot>,
    },
    /// Ask for the client chrome text a session's AI state produces (the
    /// prompt-bar context meter and the tab-inline context suffix).
    RequestAiChrome {
        session_id: SessionId,
    },
    /// Refresh the current workspace's Beads board through the real server.
    RequestBeadsBoard,
    /// Open or close job-detail interest on the daemon's capable connection.
    SetCiRunDetailsInterest {
        repo_root: PathBuf,
        head_sha: String,
        interested: bool,
    },
    /// Ask what `SessionReplay` frames the daemon has applied for a session.
    ///
    /// `min_frames` blocks until that many frames have been applied — the
    /// attach reply lands on the daemon's reader task, which otherwise races
    /// the next CLI invocation — and `timeout_ms` bounds the wait. Asking for
    /// zero frames returns the current state immediately, which is how a test
    /// states that a fresh session was never sent a replay.
    ReplayStatus {
        session_id: SessionId,
        min_frames: u32,
        timeout_ms: u64,
    },
    /// Read back the screen the daemon rebuilt from a session's replay and the
    /// live output that followed it.
    ReplayScreen {
        session_id: SessionId,
    },
    /// Compare the replayed view against a freshly requested server snapshot.
    AssertReplayMatchesScreen {
        session_id: SessionId,
    },
    /// Ask for the `WindowId` the server assigned this daemon in its `Welcome`.
    /// The visual E2E rig passes it to a GPUI client as `SCRIBE_JOIN_WINDOW` so
    /// the client joins the daemon's window share instead of opening an empty
    /// window of its own.
    WindowId,
    /// Ask for the launch (env-envelope) id the daemon minted when it created
    /// `session_id`. An E2E script asserts it is a real id to prove the harness
    /// create path names an envelope the server can persist into.
    EnvelopeId {
        session_id: SessionId,
    },
    /// Read the most recent automation action delivered to the daemon.
    LastAction,
    /// Clear the recorded automation action before an assertion phase.
    ClearAction,
    Shutdown,
}

/// Response from the daemon to a CLI subcommand.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    Ok,
    SessionCreated {
        session_id: SessionId,
    },
    ScreenshotData {
        snapshot: Box<ScreenSnapshot>,
    },
    /// The AI chrome a client would draw for a session. `prompt_bar` carries the
    /// segmented context meter (present in every band, including Ok); `tab`
    /// carries the tab-inline suffix (present only from the warn band up).
    AiChrome {
        prompt_bar: Option<String>,
        tab: Option<String>,
    },
    BeadsBoard {
        state: BeadsBoardState,
    },
    /// The window id the server assigned the daemon (`Welcome`), or an error
    /// when no `Welcome` has arrived yet.
    WindowId {
        window_id: WindowId,
    },
    /// The launch (env-envelope) id the daemon sent in the session's
    /// `CreateSession`.
    EnvelopeId {
        envelope_id: String,
    },
    /// Most recent `RunAction`, or `None` after startup/reset and before one
    /// arrives.
    LastAction {
        action: Option<AutomationAction>,
    },
    /// What the daemon has seen on a session's replay path: how many frames it
    /// applied, how many failed to inflate, the running live-output byte count,
    /// and the most recent frame.
    ReplayStatus {
        applied: u32,
        failed: u32,
        live_bytes: u64,
        last: Option<ReplayFrameInfo>,
    },
    AssertFailed {
        message: String,
    },
    Error {
        message: String,
    },
}

/// One applied `SessionReplay`, as the harness observed it.
///
/// `live_bytes_before` is what makes ordering assertable: it is the count of
/// live `PtyOutput` bytes the daemon had received for the session when the frame
/// arrived, so a caller can separate replayed content from output that followed
/// the replay instead of guessing from screen state alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFrameInfo {
    /// 1-based arrival index within the session.
    pub index: u32,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_rows: u32,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alt_screen: bool,
    /// zstd payload size on the wire.
    pub compressed_bytes: usize,
    /// ANSI byte count after inflation.
    pub inflated_bytes: usize,
    pub live_bytes_before: u64,
}

// ---------------------------------------------------------------------------
// Socket path
// ---------------------------------------------------------------------------

/// Returns the Unix-domain socket path for the test daemon.
///
/// Path: `/run/user/{uid}/scribe/test-daemon.sock` for a stable build, or the
/// `scribe-dev` runtime directory when this binary runs under that file stem.
/// It is derived from the server socket rather than hardcoded so a harness
/// staged onto the dev install — as the perf A/B rig stages every binary it
/// launches — cannot end up with its control socket in one install's runtime
/// directory and its sessions in another's.
pub fn daemon_socket_path() -> PathBuf {
    let uid = nix::unistd::geteuid();
    scribe_common::socket::server_socket_path().parent().map_or_else(
        || PathBuf::from(format!("/run/user/{uid}/scribe/test-daemon.sock")),
        |dir| dir.join("test-daemon.sock"),
    )
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
