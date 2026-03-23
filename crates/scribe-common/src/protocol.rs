use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::AiProcessState;
use crate::ids::{SessionId, WorkspaceId};

// ── UI → Server ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    KeyInput {
        session_id: SessionId,
        data: Vec<u8>,
    },
    Resize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    CreateSession {
        workspace_id: WorkspaceId,
    },
    CloseSession {
        session_id: SessionId,
    },
    CreateWorkspace,
    CloseWorkspace {
        workspace_id: WorkspaceId,
    },
    MoveSession {
        session_id: SessionId,
        target_workspace: WorkspaceId,
    },
    ScrollRequest {
        session_id: SessionId,
        offset: i32,
    },
    Subscribe {
        session_ids: Vec<SessionId>,
    },
    RequestSnapshot {
        session_id: SessionId,
    },
    /// Request a list of all live sessions on the server.
    ListSessions,
    /// Attach to existing (detached) sessions, taking ownership.
    AttachSessions {
        session_ids: Vec<SessionId>,
    },
    /// Notify server that config file has been updated.
    ConfigReloaded,
}

// ── Server → UI ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Fast path: raw PTY output bytes.
    PtyOutput {
        session_id: SessionId,
        data: Vec<u8>,
    },
    /// Full screen state for reconnect or tab switch.
    ScreenSnapshot {
        session_id: SessionId,
        snapshot: crate::screen::ScreenSnapshot,
    },
    AiStateChanged {
        session_id: SessionId,
        ai_state: AiProcessState,
    },
    CwdChanged {
        session_id: SessionId,
        cwd: PathBuf,
    },
    TitleChanged {
        session_id: SessionId,
        title: String,
    },
    WorkspaceNamed {
        workspace_id: WorkspaceId,
        name: String,
    },
    SessionCreated {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        /// Basename of the shell binary (e.g. "zsh", "bash").
        shell_name: String,
    },
    SessionExited {
        session_id: SessionId,
        exit_code: Option<i32>,
    },
    Bell {
        session_id: SessionId,
    },
    Error {
        message: String,
    },
    /// Git branch for the session's CWD (None if not in a git repo).
    GitBranch {
        session_id: SessionId,
        branch: Option<String>,
    },
    /// List of all live sessions, sent in response to `ListSessions`.
    SessionList {
        sessions: Vec<SessionInfo>,
    },
    /// Full workspace state sent to client on creation or reconnect.
    WorkspaceInfo {
        workspace_id: WorkspaceId,
        name: Option<String>,
        /// Hex color string (e.g. "#a78bfa") from the rotating accent palette.
        accent_color: String,
    },
}

// ── Shared types ─────────────────────────────────────────────────

/// Summary of a live session, sent in `SessionList` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
}
