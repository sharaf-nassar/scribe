use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::AiProcessState;
use crate::ids::{SessionId, WindowId, WorkspaceId};

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
        /// When this session creates a new workspace (via a split), the
        /// direction of that split.  `None` when adding a tab to an
        /// existing workspace.
        split_direction: Option<LayoutDirection>,
        /// Working directory for the new shell.  When `Some`, the PTY is
        /// spawned in this directory (used to inherit the active tab's CWD).
        /// `None` falls back to `$HOME`.
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// Initial terminal dimensions.  When provided the PTY is created at
        /// this size instead of the 80×24 default, avoiding a resize race
        /// where the shell's first output is formatted for the wrong width.
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
        /// Optional command to run instead of the default shell.
        /// When `Some`, the PTY spawns this command directly (e.g. `["claude"]`).
        /// The first element is the program, remaining elements are arguments.
        #[serde(default)]
        command: Option<Vec<String>>,
    },
    CloseSession {
        session_id: SessionId,
    },
    CreateWorkspace,
    /// Close a workspace by ID.
    ///
    /// TODO: not yet implemented on the server side — the server does not currently
    /// handle this variant.
    CloseWorkspace {
        workspace_id: WorkspaceId,
    },
    /// Move a session to a different workspace.
    ///
    /// TODO: not yet implemented on the server side — the server does not currently
    /// handle this variant.
    MoveSession {
        session_id: SessionId,
        target_workspace: WorkspaceId,
    },
    /// Request a scrollback snapshot at a given offset from the bottom.
    ///
    /// TODO: not yet implemented on the server side — the server does not currently
    /// handle this variant.
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
    ///
    /// When `dimensions` is provided, the server resizes each session's `Term`
    /// and PTY to the given `(cols, rows)` before capturing the screen snapshot,
    /// avoiding a dimension mismatch on reconnect.  The length of `dimensions`
    /// must match the length of `session_ids`.
    AttachSessions {
        session_ids: Vec<SessionId>,
        /// Per-session dimensions `(cols, rows)` parallel to `session_ids`.
        #[serde(default)]
        dimensions: Vec<(u16, u16)>,
    },
    /// Notify server that config file has been updated.
    ConfigReloaded,
    /// Report the current workspace split tree so the server can persist it
    /// for reconnect and handoff.  Sent by the client after every tree
    /// mutation (split, close, divider drag).
    ReportWorkspaceTree {
        tree: WorkspaceTreeNode,
    },
    /// Search for text in the terminal scrollback/screen.
    ///
    /// TODO: not yet implemented on the server side — the server does not currently
    /// handle this variant.
    SearchRequest {
        session_id: SessionId,
        query: String,
        /// Maximum number of matches to return.
        limit: u32,
    },
    /// First message after connect — identifies this window to the server.
    /// `None` means the client is starting fresh and the server should assign
    /// or create a window.
    Hello {
        window_id: Option<WindowId>,
    },
    /// Close this window and destroy all its sessions.  Sent when the user
    /// chooses "Close this window only" from the close dialog.
    CloseWindow {
        window_id: WindowId,
    },
    /// Request all connected clients to save state and close gracefully.
    QuitAll,
    /// User confirmed the update — download and install.
    TriggerUpdate,
    /// User dismissed the update notification.
    DismissUpdate,
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
    AiStateCleared {
        session_id: SessionId,
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
        /// Full workspace split tree, if one has been reported by a client.
        /// `None` when no client has connected yet or when upgrading from an
        /// older server that did not persist the tree.
        workspace_tree: Option<WorkspaceTreeNode>,
    },
    /// Full workspace state sent to client on creation or reconnect.
    WorkspaceInfo {
        workspace_id: WorkspaceId,
        name: Option<String>,
        /// Hex color string (e.g. "#a78bfa") from the rotating accent palette.
        accent_color: String,
        /// Direction of the split that created this workspace.  `None` for
        /// the initial (unsplit) workspace.
        split_direction: Option<LayoutDirection>,
    },
    /// Scrollback snapshot at a specific offset from the bottom.
    ScrolledSnapshot {
        session_id: SessionId,
        snapshot: crate::screen::ScreenSnapshot,
        /// The actual offset applied (clamped by available history).
        applied_offset: u32,
    },
    /// Search results for a `SearchRequest`.
    SearchResults {
        session_id: SessionId,
        query: String,
        matches: Vec<SearchMatch>,
    },
    /// Response to `Hello` — confirms the assigned window ID and lists other
    /// windows that need to be spawned (for session restoration on startup).
    Welcome {
        window_id: WindowId,
        /// Window IDs that have detached sessions but no connected client.
        /// The receiving client should spawn a new process for each.
        other_windows: Vec<WindowId>,
    },
    /// Server requests this client to save state and close gracefully.
    /// Sent in response to another client's `QuitAll`.
    QuitRequested,
    /// A newer version is available for download.
    UpdateAvailable {
        version: String,
        release_url: String,
    },
    /// Progress update during download/install.
    UpdateProgress {
        state: UpdateProgressState,
    },
}

// ── Shared types ─────────────────────────────────────────────────

/// Direction of a workspace split, persisted by the server so the client
/// can reconstruct the window layout on reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutDirection {
    /// Side-by-side (left | right).
    Horizontal,
    /// Top-over-bottom (top / bottom).
    Vertical,
}

/// Serialisable workspace split tree.
///
/// Contains only the structural information the server needs to store and
/// relay so the client can reconstruct its `WindowNode` tree exactly on
/// reconnect: split direction, split ratio, and workspace leaf IDs.
///
/// Tab/pane state, accent colours, and names are NOT part of this tree —
/// those travel in `WorkspaceInfo` messages and the flat workspace map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkspaceTreeNode {
    /// A single workspace occupying its entire region.
    Leaf { workspace_id: WorkspaceId },
    /// A split dividing space between two sub-trees.
    Split {
        direction: LayoutDirection,
        /// Fraction of space allocated to `first` (0.0–1.0).
        ratio: f32,
        first: Box<WorkspaceTreeNode>,
        second: Box<WorkspaceTreeNode>,
    },
}

/// Summary of a live session, sent in `SessionList` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    /// Last-known terminal title (from OSC 0/2). `None` before first title event.
    pub title: Option<String>,
    /// Last-known working directory (from OSC 7). `None` before first CWD event.
    pub cwd: Option<PathBuf>,
    /// Last-known AI process state (from OSC 1337). `None` when no AI is active.
    #[serde(default)]
    pub ai_state: Option<AiProcessState>,
}

/// A single search match location in the terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
}

/// Progress state for an in-flight update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateProgressState {
    /// Downloading the update package.
    Downloading,
    /// Verifying the cryptographic signature.
    Verifying,
    /// Installing the update package.
    Installing,
    /// Installation completed successfully. Client should restart (macOS) or
    /// sessions will hot-reload automatically (Linux).
    Completed { version: String },
    /// An error occurred during the update process.
    Failed { reason: String },
}
