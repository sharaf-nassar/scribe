use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::{AiProcessState, AiProvider};
use crate::ids::{SessionId, WindowId, WorkspaceId};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
}

impl TerminalSize {
    #[must_use]
    pub fn has_grid(self) -> bool {
        self.cols > 0 && self.rows > 0
    }

    #[must_use]
    pub fn has_pixels(self) -> bool {
        self.cell_width > 0 && self.cell_height > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomationAction {
    OpenSettings,
    OpenFind,
    NewTab,
    NewClaudeTab,
    NewClaudeResumeTab,
    NewCodexTab,
    NewCodexResumeTab,
    SplitVertical,
    SplitHorizontal,
    ClosePane,
    CloseTab,
    NewWindow,
    SwitchProfile { name: String },
    OpenUpdateDialog,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: WindowId,
    pub session_count: usize,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionContext {
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub tmux_session: Option<String>,
}

// ── UI → Server ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    KeyInput {
        session_id: SessionId,
        data: Vec<u8>,
        /// Whether this input should dismiss client-attention AI states such
        /// as waiting-for-input and permission prompts.
        #[serde(default)]
        dismisses_attention: bool,
    },
    Resize {
        session_id: SessionId,
        size: TerminalSize,
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
        size: Option<TerminalSize>,
        /// Optional command to run instead of the default shell.
        /// When `Some`, the PTY spawns this command directly (e.g. `["codex"]`).
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
    /// When `dimensions` is non-empty, the server resizes each session's
    /// Term and PTY to the given terminal size **before** taking the
    /// snapshot.  This ensures the snapshot matches the client's pane
    /// grid and avoids a post-attach SIGWINCH that corrupts content.
    AttachSessions {
        session_ids: Vec<SessionId>,
        /// Per-session terminal sizes parallel to `session_ids`.
        #[serde(default)]
        dimensions: Vec<TerminalSize>,
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
    /// Request a list of windows known to the server.
    ListWindows,
    /// Ask a connected client window to run an automation action.
    DispatchAction {
        window_id: Option<WindowId>,
        action: AutomationAction,
    },
    /// Notify server of pane focus change so it can send CSI focus events
    /// to PTY applications that have enabled DECSET 1004 (`FOCUS_IN_OUT`).
    FocusChanged {
        /// Session that gained focus. `None` when window lost OS focus.
        gained: Option<SessionId>,
        /// Session that lost focus. `None` when window gained OS focus
        /// (previous focus is unknown from the first focus event).
        lost: Option<SessionId>,
    },
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
    SessionContextChanged {
        session_id: SessionId,
        context: SessionContext,
    },
    TitleChanged {
        session_id: SessionId,
        title: String,
    },
    CodexTaskLabelChanged {
        session_id: SessionId,
        task_label: String,
    },
    CodexTaskLabelCleared {
        session_id: SessionId,
    },
    /// A user prompt was submitted in a Claude Code or Codex session.
    PromptReceived {
        session_id: SessionId,
        provider: AiProvider,
        text: String,
    },
    WorkspaceNamed {
        workspace_id: WorkspaceId,
        name: String,
        /// Absolute path to the project directory (root + first CWD component).
        #[serde(default)]
        project_root: Option<PathBuf>,
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
        /// Absolute path to the project directory (root + first CWD component).
        #[serde(default)]
        project_root: Option<PathBuf>,
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
    /// Confirms that the server permanently removed a window and its sessions.
    WindowClosed {
        window_id: WindowId,
    },
    /// List of windows known to the server and whether they are connected.
    WindowList {
        windows: Vec<WindowInfo>,
    },
    /// Request for a connected client window to execute an automation action.
    RunAction {
        action: AutomationAction,
    },
    /// Confirms that a requested automation action was routed to a target window.
    ActionDispatched {
        window_id: WindowId,
    },
    /// Server requests this client to save state and close gracefully.
    /// Sent in response to a client's `QuitAll`, including the sender.
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
    /// A shell prompt-mark event from OSC 133.
    PromptMark {
        session_id: SessionId,
        kind: PromptMarkKind,
        /// Whether the shell requested click-to-move (OSC 133;A with `click_events=1`).
        click_events: bool,
        /// Exit code from the previous command (only for `CommandEnd` / D mark).
        exit_code: Option<i32>,
    },
    /// The server suppressed an ED 3 (clear scrollback) sequence from an AI
    /// session.  The client should reset `display_offset` to 0 so the
    /// viewport snaps to the live terminal, matching the scroll-to-bottom
    /// side-effect of a real ED 3.
    ScrollBottom {
        session_id: SessionId,
    },
}

// ── Shared types ─────────────────────────────────────────────────

/// Shell prompt-mark variant from OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptMarkKind {
    /// OSC 133;A — prompt start.
    PromptStart,
    /// OSC 133;B — prompt end / command start.
    PromptEnd,
    /// OSC 133;C — command start (after prompt).
    CommandStart,
    /// OSC 133;D — command end (with optional exit code).
    CommandEnd,
}

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
    Leaf {
        workspace_id: WorkspaceId,
        /// Ordered session IDs for tabs in this workspace.
        /// Populated by client when reporting tree; empty when received from server.
        #[serde(default)]
        session_ids: Vec<SessionId>,
        /// Per-tab pane layout trees, parallel to `session_ids`.
        /// `None` entries represent single-pane tabs (the default).
        /// Empty vec means all tabs are single-pane (backward compat).
        #[serde(default)]
        pane_trees: Vec<Option<PaneTreeNode>>,
    },
    /// A split dividing space between two sub-trees.
    Split {
        direction: LayoutDirection,
        /// Fraction of space allocated to `first` (0.0–1.0).
        ratio: f32,
        first: Box<WorkspaceTreeNode>,
        second: Box<WorkspaceTreeNode>,
    },
}

/// Serialisable pane split tree within a single tab.
///
/// Each leaf holds the session ID of the pane's PTY session. Split nodes
/// describe how the tab's content area is divided.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaneTreeNode {
    /// A single pane occupying the full tab content area.
    Leaf { session_id: SessionId },
    /// A split dividing the tab content area between two sub-trees.
    Split {
        direction: LayoutDirection,
        ratio: f32,
        first: Box<PaneTreeNode>,
        second: Box<PaneTreeNode>,
    },
}

/// Summary of a live session, sent in `SessionList` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    /// Basename of the session shell or command entrypoint.
    pub shell_name: String,
    /// Last-known terminal title (from OSC 0/2). `None` before first title event.
    pub title: Option<String>,
    /// Last-known shell/session context (remote host, tmux session).
    #[serde(default)]
    pub context: Option<SessionContext>,
    /// Last-known Codex task label. `None` when the session is not showing one.
    #[serde(default)]
    pub codex_task_label: Option<String>,
    /// Last-known working directory (from OSC 7). `None` before first CWD event.
    pub cwd: Option<PathBuf>,
    /// Last-known AI process state (from OSC 1337). `None` when no AI is active.
    #[serde(default)]
    pub ai_state: Option<AiProcessState>,
    /// Last-known AI provider for the session even when there is no active
    /// visible AI state. Used to preserve provider-aware client behavior on
    /// reconnect after an attention state was dismissed locally.
    #[serde(default)]
    pub ai_provider_hint: Option<AiProvider>,
}

/// A single search match location in the terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub row: i32,
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
    /// Installation succeeded but the automatic restart failed; the user must
    /// restart manually to apply the update.
    CompletedRestartRequired { version: String },
    /// An error occurred during the update process.
    Failed { reason: String },
}
