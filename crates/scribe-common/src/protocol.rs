use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::AiProcessState;
use crate::ids::{SessionId, WorkspaceId};

// ── UI → Server ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    KeyInput { session_id: SessionId, data: Vec<u8> },
    Resize { session_id: SessionId, cols: u16, rows: u16 },
    CreateSession { workspace_id: WorkspaceId },
    CloseSession { session_id: SessionId },
    CreateWorkspace,
    CloseWorkspace { workspace_id: WorkspaceId },
    MoveSession { session_id: SessionId, target_workspace: WorkspaceId },
    ScrollRequest { session_id: SessionId, offset: i32 },
    Subscribe { session_ids: Vec<SessionId> },
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
}
