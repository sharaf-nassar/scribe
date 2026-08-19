use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::agent::{AgentActionOutcome, AgentCapability, AgentRequest, AgentResponse};
use crate::ai_state::{AiProcessState, AiProvider, AiState};
use crate::config::SharingMode;
use crate::hook;
use crate::ids::{SessionId, WindowId, WorkspaceId};
use crate::terminal_images::{
    RemoteProtocolMismatch, TerminalImageCapabilities, TerminalImageCapabilityMismatch,
    TerminalImageLiveMessage, TerminalImageReplayMessage,
};

/// Version of the remote-only wire protocol. Exchanged in the tailnet
/// [`ClientMessage::RemoteHandshake`] preamble (feature 013) and the LAN
/// [`ClientMessage::LanHello`] preamble (feature 014), and gated by an
/// exact-match check answered in [`ServerMessage::RemoteHandshakeReply`]
/// (tailnet) or reported as [`LanRefusal::IncompatibleVersion`] inside
/// [`ServerMessage::LanApprovalResult`] (LAN); bump on ANY change to
/// remote-visible message semantics. Never used on the local Unix socket, where
/// client and server always ship together.
///
/// Bumped to `2` for feature 014: the LAN device-approval and discovery/trust
/// messages are additive under the same exact-match policy, so a 013-only peer
/// and a 013+014 peer never interoperate over a remote transport.
///
/// Bumped to `3` for feature 015 (multi-machine collaborative window sharing):
/// the new share-control [`ClientMessage`] variants and roster/notice
/// [`ServerMessage`] variants are additive under the same exact-match policy, so
/// a v2 peer and a v3 peer never share a window (FR-014, D4).
///
/// Bumped to `4` for feature 018: [`ClientMessage::CreateSession`] gains the
/// additive structured AI-launch request used by server-owned command building.
///
/// Bumped to `5` for terminal-images v1: remote peers must share the typed
/// image capability, live-update, and replay contract exactly.
///
/// Bumped to `6` for the remote-visible CI run state and dismissal messages.
///
/// Bumped to `7` because suppressed AI ED 3 no longer emits `ScrollBottom`,
/// changing the terminal-frame semantics visible to remote peers.
///
/// Bumped to `8` because remote session metadata may now carry
/// [`AiProvider::Pi`].
///
/// Bumped to `9` for the Beads Flow view: the epic-graph request/reply pair and
/// the `beads_flow` capability are additive under the same exact-match policy,
/// so a v8 peer never negotiates a graph it cannot render.
pub const REMOTE_PROTOCOL_VERSION: u32 = 9;

/// OSC 52 operation type (spec 010 E2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOp {
    Read,
    Write,
}

/// OSC 52 selection target (spec 010 E2). Per FR-004 / FR-011 both axes share
/// the same policy mode; `Primary` resolves to the system clipboard on
/// non-X11 platforms at the `arboard` layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

/// User decision returned from the OSC 52 confirmation overlay (spec 010 E2).
/// `AlwaysAllow` / `AlwaysDeny` trigger a persisted policy update on the
/// originating axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDecision {
    AllowOnce,
    DenyOnce,
    AlwaysAllow,
    AlwaysDeny,
}

/// Why the client-side OSC 52 host clipboard bridge could not service a
/// request (spec 010 E2). Mapped server-side onto an empty OSC 52 reply per
/// UX-002.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeError {
    /// `arboard` failed to initialize or returned an error (compositor
    /// restart, X11 selection ownership lost, etc.).
    Unavailable,
    /// The host clipboard contained no usable text payload.
    Empty,
}

/// Opaque identifier for an in-flight OSC 52 confirmation prompt (spec 010).
///
/// Serializes transparently as a `u64`; the server-side issuer is responsible
/// for monotonic allocation and uniqueness within a session lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PromptId(pub u64);

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
    SwitchProfile {
        name: String,
    },
    OpenUpdateDialog,
    /// Raise the window and switch to the tab containing this session.
    FocusSession {
        session_id: SessionId,
    },
}

/// Feature 013: identity of the controller currently holding a window,
/// surfaced in window-listing / picker / indicator UIs (FR-005, FR-009b,
/// SC-006). Present on a [`WindowInfo`] only while a REMOTE tailnet peer
/// controls the window; a locally-controlled or unconnected window carries
/// `None` (the owning machine needs no device/account label for itself).
/// Mirrors the `device_name` / `login_name` pair of
/// [`ServerMessage::WindowTakenOver`] so the two identity surfaces never drift.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControllerInfo {
    /// Controller's `MagicDNS` short device name.
    pub device_name: String,
    /// Controller's tailnet account display (login) name.
    pub login_name: String,
}

/// Feature 015: one attached participant in a shared window, as broadcast in a
/// [`ServerMessage::ShareRoster`] (contracts/remote-protocol-v3.md). Reuses the
/// `device_name` / `login_name` identity pair of [`ControllerInfo`] so the
/// identity surface never drifts, plus the roster-only `is_local` / `is_holder`
/// flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParticipantInfo {
    /// Server-monotonic participant id, stable for the connection's lifetime;
    /// the target of a [`ClientMessage::ControlGrant`].
    pub participant_id: u64,
    /// Participant's short device name.
    pub device_name: String,
    /// Participant's account display (login) name.
    pub login_name: String,
    /// `true` for the owning (local) machine's own participant.
    pub is_local: bool,
    /// `true` for the current input-control holder (single-typist mode).
    pub is_holder: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: WindowId,
    pub session_count: usize,
    pub connected: bool,
    /// Feature 013: display names of the workspaces in this window, in the
    /// window's stored session order (deduplicated; unnamed workspaces
    /// omitted). Feeds the remote connect picker's window list (FR-005).
    /// Empty from an older server or a window with no named workspaces.
    #[serde(default)]
    pub workspace_names: Vec<String>,
    /// Feature 013: the remote controller currently holding this window, or
    /// `None` when it is unconnected or controlled locally (FR-009b, SC-006).
    /// Additive: older servers omit it and it decodes as `None`. In feature 015
    /// shared modes this names the current input-control holder (or `None` when
    /// unheld); in `SingleController` mode it still names the sole holder.
    #[serde(default)]
    pub controller: Option<ControllerInfo>,
    /// Feature 015: remote participants attached to this shared window; empty
    /// from an older server or a locally-controlled / unconnected window
    /// (contracts/remote-protocol-v3.md). Reuses [`ControllerInfo`] per entry.
    #[serde(default)]
    pub participants: Vec<ControllerInfo>,
    /// Feature 015: the window's sharing mode; `None` decodes from an older
    /// server that predates sharing.
    #[serde(default)]
    pub mode: Option<SharingMode>,
    /// Feature 015: number of attached participants, so the connect picker can
    /// show share occupancy instead of feature 013's binary in-use flag.
    #[serde(default)]
    pub participant_count: usize,
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

/// Whether an AI launch starts a new conversation or resumes one.
///
/// This type is also persisted in client restore-state TOML. Its serde variant
/// names are therefore intentionally the Rust names `New` and `Resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiResumeMode {
    New,
    Resume,
}

/// Structured AI launch intent for server-owned provider command construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiLaunchSpec {
    pub provider: AiProvider,
    pub resume_mode: AiResumeMode,
    pub conversation_id: Option<String>,
}

/// A launch-only CLI a tab runs after its shell's startup files.
///
/// Deliberately *not* an [`AiProvider`]: a shell tool has no hook channel, no
/// conversation, and no resume mode, so it is never tracked as AI chrome. The
/// wire carries the variant rather than a binary name so the server's shell
/// command string can never be composed from client-supplied text.
///
/// This type is also persisted in client restore-state TOML, hence the
/// explicit `snake_case` renaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTool {
    Pi,
}

impl ShellTool {
    /// The binary the tab execs.
    #[must_use]
    pub fn binary_name(self) -> &'static str {
        match self {
            ShellTool::Pi => "pi",
        }
    }
}

/// Build Pi launch metadata for a peer's negotiated local capability.
///
/// Unsupported peers keep the legacy [`ShellTool::Pi`] representation so they
/// never deserialize the newer [`AiProvider::Pi`] enum value.
#[must_use]
pub fn pi_launch_metadata(pi_provider: bool) -> (Option<AiLaunchSpec>, Option<ShellTool>) {
    if pi_provider {
        (
            Some(AiLaunchSpec {
                provider: AiProvider::Pi,
                resume_mode: AiResumeMode::New,
                conversation_id: None,
            }),
            None,
        )
    } else {
        (None, Some(ShellTool::Pi))
    }
}

/// Version of the local workspace Beads-board snapshot payload.
///
/// Detail reads use separate named messages, so adding them does not change
/// the board snapshot schema or require a version bump.
pub const BEADS_BOARD_PROTOCOL_VERSION: u32 = 1;

/// One issue rendered by the workspace Beads board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsBoardItem {
    pub id: String,
    pub title: String,
    /// Native Beads priority (`0` is highest, `4` is lowest).
    pub priority: u8,
    #[serde(default)]
    pub blocker_ids: Vec<String>,
    #[serde(default)]
    pub parent_epic_name: Option<String>,
    /// Epic id the display name is derived from. This is what decides Flow
    /// eligibility client-side: no parent epic means no graph to open.
    #[serde(default)]
    pub parent_epic_id: Option<String>,
}

/// A complete, mutually-exclusive five-queue board snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsBoardSnapshot {
    pub refreshed_at_epoch_ms: u64,
    #[serde(default)]
    pub backlog_total: u32,
    #[serde(default)]
    pub ready_total: u32,
    #[serde(default)]
    pub in_progress_total: u32,
    #[serde(default)]
    pub blocked_total: u32,
    #[serde(default)]
    pub done_total: u32,
    pub backlog: Vec<BeadsBoardItem>,
    pub ready: Vec<BeadsBoardItem>,
    pub in_progress: Vec<BeadsBoardItem>,
    pub blocked: Vec<BeadsBoardItem>,
    pub done: Vec<BeadsBoardItem>,
}

/// Server-owned loading state for one workspace's Beads board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BeadsBoardState {
    /// `bd context` found no Beads project for this workspace.
    NotDetected,
    /// A refresh is running. A previous memory snapshot may already be paintable.
    Loading {
        #[serde(default)]
        cached: Option<BeadsBoardSnapshot>,
    },
    /// Last-good snapshot. `stale` means a refresh is due or failed.
    Ready {
        snapshot: BeadsBoardSnapshot,
        stale: bool,
        #[serde(default)]
        refresh_error: Option<String>,
    },
    /// `bd` could not be invoked and no last-good snapshot exists.
    Unavailable { message: String },
}

/// Queue assigned by the server using the board classifier's precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsIssueQueue {
    Backlog,
    Ready,
    InProgress,
    Blocked,
    Done,
}

/// Fact that caused the server to assign an issue to its queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsIssueQueueBasis {
    ClosedStatus,
    BlockedStatus,
    OpenBlockers,
    InProgressStatus,
    ReadySet,
    BacklogFallback,
}

/// One issue related to the detailed issue through a blocking edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsIssueLink {
    pub id: String,
    pub title: String,
}

/// One newest-first comment in a detailed issue response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsIssueComment {
    pub author: String,
    /// ISO-8601 timestamp from `bd`, kept verbatim.
    pub created_at: String,
    pub body: String,
}

/// Complete, bounded issue data returned by a detail read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsIssueDetail {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub notes: String,
    pub design: String,
    pub spec_id: Option<String>,
    pub status: String,
    /// Native Beads priority (`0` is highest, `4` is lowest).
    pub priority: u8,
    pub issue_type: String,
    pub labels: Vec<String>,
    /// Parent epic title resolved by the server from the `parent-child` relation.
    #[serde(default)]
    pub parent_epic_name: Option<String>,
    pub assignee: Option<String>,
    pub owner: Option<String>,
    /// ISO-8601 timestamp from `bd`, kept verbatim.
    pub created_at: String,
    /// ISO-8601 timestamp from `bd`, kept verbatim.
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub defer_until: Option<String>,
    pub due_at: Option<String>,
    pub estimated_minutes: Option<u32>,
    pub external_ref: Option<String>,
    pub blockers: Vec<BeadsIssueLink>,
    pub dependents: Vec<BeadsIssueLink>,
    /// Newest 50 comments at most, in newest-first order.
    pub comments: Vec<BeadsIssueComment>,
    /// Older comments omitted from `comments` by the server cap.
    pub hidden_comment_count: u32,
    pub queue: BeadsIssueQueue,
    pub queue_basis: BeadsIssueQueueBasis,
}

/// One server-side `bd` write selected by client chrome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsIssueWrite {
    SetTitle { title: String },
    SetDescription { description: String },
    SetAcceptance { acceptance: String },
    SetNotes { notes: String },
    SetDesign { design: String },
    SetSpecId { spec_id: Option<String> },
    SetPriority { priority: u8 },
    SetType { issue_type: String },
    SetLabels { labels: Vec<String> },
    SetStatus { status: String, clear_defer: bool },
    Claim,
    CloseIssue,
    UndoClose,
    AddComment { body: String },
}

/// Optional optimistic-concurrency checks captured from a fresh detail read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsIssueWriteGuards {
    #[serde(default)]
    pub if_status: Option<String>,
    #[serde(default)]
    pub if_assignee: Option<String>,
}

/// Outcome of one typed issue write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsIssueWriteResult {
    Applied { generation: u64 },
    PreconditionFailed,
    Failed { reason: String },
}

/// One issue in an epic's dependency graph.
///
/// Deliberately narrower than [`BeadsIssueDetail`]: the Flow view paints a
/// compact node and opens the existing detail panel for everything else, so
/// the graph carries only what a node renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsGraphNode {
    pub id: String,
    pub title: String,
    /// Native Beads priority (`0` is highest, `4` is lowest).
    pub priority: u8,
    pub status: String,
    /// Queue assigned by the same classifier the board uses, so a node's state
    /// treatment cannot disagree with the lane the card came from.
    pub queue: BeadsIssueQueue,
    #[serde(default)]
    pub assignee: Option<String>,
    /// ISO-8601 timestamp from `bd`, kept verbatim.
    pub updated_at: String,
}

/// One `blocks` edge between two members of the same epic.
///
/// `parent-child` defines epic membership, not adjacency, so it never appears
/// here. Satisfied (closed-blocker) edges are included: the graph shows what a
/// node waited on, not only what still blocks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsGraphEdge {
    /// Blocker: the issue that must finish first.
    pub from: String,
    /// Dependent: the issue held up by `from`.
    pub to: String,
}

/// The most nodes one epic graph may carry.
///
/// Both ends share this constant because it is a wire bound, not a local
/// policy: were the server to admit a graph the client refuses, the strip
/// would drop it with nothing but a debug line to explain the blank.
pub const MAX_FLOW_NODES: usize = 200;

/// A complete, admitted epic dependency graph.
///
/// There is deliberately no `truncated` flag. An epic exceeding the server's
/// bound is refused outright ([`BeadsEpicGraphRefusal::TooLarge`]) rather than
/// served partial, so a cursor node can never be cut out of its own graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsEpicGraph {
    pub epic_id: String,
    pub epic_title: String,
    /// Members already closed, for the band's progress readout.
    pub closed: u32,
    /// Total members, equal to `nodes.len()` — carried explicitly so the band
    /// does not have to re-derive it.
    pub total: u32,
    pub nodes: Vec<BeadsGraphNode>,
    pub edges: Vec<BeadsGraphEdge>,
}

/// Why the server declined to serve a graph for an otherwise valid request.
///
/// The client renders none of these — every refusal means "stay in Lanes" —
/// but the reason is logged server-side so an epic that never opens is
/// diagnosable instead of mysterious.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsEpicGraphRefusal {
    /// The requested id names no epic, or the epic has no members.
    NoEpic,
    /// The `blocks` edges contain a cycle, so no layered ranking exists.
    Cycle,
    /// A member shares no edge with any other member.
    Disconnected,
    /// A member is blocked by an issue outside the epic.
    ExternalBlocker,
    /// The epic exceeds the node or per-node edge bound.
    TooLarge,
}

/// Correlated reply to [`ClientMessage::RequestBeadsEpicGraph`].
///
/// Typed rather than an `Option` so a refusal carries its reason: the three
/// arms separate an admitted graph, a deliberate refusal, and a failed read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadsEpicGraphOutcome {
    Graph(Box<BeadsEpicGraph>),
    NoGraph { reason: BeadsEpicGraphRefusal },
    Unavailable { message: String },
}

/// Aggregate state rendered by the CI run bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiRunStatus {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
}

/// GitHub workflow execution phase before its terminal conclusion is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiWorkflowStatus {
    Queued,
    InProgress,
    Completed,
}

/// Terminal workflow result used by the aggregate worst-status rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiRunConclusion {
    Success,
    Failure,
    Cancelled,
}

/// One workflow run contributing to a pushed head's aggregate CI state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiWorkflowRun {
    pub run_id: u64,
    pub name: String,
    pub status: CiWorkflowStatus,
    pub conclusion: Option<CiRunConclusion>,
    pub started_at_epoch_secs: Option<u64>,
    pub updated_at_epoch_secs: Option<u64>,
}

/// Full bounded snapshot for one repository's currently tracked pushed head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRunState {
    /// Trusted GitHub `owner/name`, enough for the owning client to open a run.
    pub repository: String,
    pub head_sha: String,
    pub branch: String,
    pub workflows: Vec<CiWorkflowRun>,
    pub rollup: CiRunStatus,
    /// The tracker could not refresh an active run; last-known data remains valid.
    pub stale: bool,
}

/// Replacement-sized CI updates. A clear names the head it retires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CiRunDelta {
    Set(CiRunState),
    Cleared { head_sha: String },
}

/// One named step inside a workflow job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJobStep {
    pub name: String,
    pub status: CiWorkflowStatus,
    pub conclusion: Option<CiRunConclusion>,
}

/// One job in the expanded CI trace panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    pub job_id: u64,
    pub workflow_run_id: u64,
    pub workflow_name: String,
    pub name: String,
    pub status: CiWorkflowStatus,
    pub conclusion: Option<CiRunConclusion>,
    pub started_at_epoch_secs: Option<u64>,
    pub completed_at_epoch_secs: Option<u64>,
    pub steps: Vec<CiJobStep>,
}

/// Head-qualified job detail sent only to clients with an open trace panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRunDetails {
    pub head_sha: String,
    pub jobs: Vec<CiJob>,
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
        /// Structured AI launch intent. AI clients leave `command` empty when
        /// this is present; the server owns shell resolution and argv.
        #[serde(default)]
        ai_launch: Option<AiLaunchSpec>,
        /// Launch-only tool intent. Like `ai_launch` the server owns shell
        /// resolution and argv, so `command` stays empty; unlike it the tool
        /// is never tracked as an AI provider.
        #[serde(default)]
        shell_tool: Option<ShellTool>,
        /// Cold-restart restore association: the `LaunchRecord.launch_id` whose
        /// persisted env envelope (if present) should be decrypted and applied
        /// to this freshly-spawned PTY. `None` for plain new sessions.
        #[serde(default)]
        env_envelope_id: Option<String>,
    },
    CloseSession {
        session_id: SessionId,
    },
    CreateWorkspace,
    /// Close a workspace by ID — sent when its last region collapses. The
    /// client closes the region's sessions first, so this normally names an
    /// already-empty workspace.
    CloseWorkspace {
        workspace_id: WorkspaceId,
    },
    /// Request the cached Beads board for one authoritative server workspace.
    RequestBeadsBoard {
        workspace_id: WorkspaceId,
        protocol_version: u32,
    },
    /// Request an uncached, complete issue read for one workspace board.
    RequestBeadsIssueDetail {
        workspace_id: WorkspaceId,
        issue_id: String,
    },
    /// Request the dependency graph for one epic, to open the Flow view.
    ///
    /// Separately bounded from the board snapshot so the board's per-queue cap
    /// cannot hole an epic whose closed members fall past it.
    RequestBeadsEpicGraph {
        workspace_id: WorkspaceId,
        epic_id: String,
    },
    /// Request one typed issue mutation. The server owns `bd` argv composition.
    BeadsIssueWrite {
        workspace_id: WorkspaceId,
        issue_id: String,
        verb: BeadsIssueWrite,
        guards: BeadsIssueWriteGuards,
    },
    /// Move a session to a different workspace. The workspace-split flow seeds
    /// its session through the old workspace (the new one does not exist yet)
    /// and sends this once the new region's pane adopts it, re-keying the
    /// server's membership so `SessionList`, CWD auto-naming, and handoff
    /// persistence agree with the client's regions.
    MoveSession {
        session_id: SessionId,
        target_workspace: WorkspaceId,
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
    /// The find overlay closed: the server may drop the scrollback snapshot it
    /// cached to answer this session's query edits (spec 017 US8-2).
    ///
    /// Purely an advisory release. A server that never receives it still drops
    /// the snapshot on the session's next output, and a client that never
    /// sends it only delays that release.
    SearchClosed {
        session_id: SessionId,
    },
    /// First message after connect — identifies this window to the server.
    /// `None` means the client is starting fresh and the server should assign
    /// or create a window.
    Hello {
        window_id: Option<WindowId>,
        /// Spec 010: client advertises OSC 52 clipboard-gating support. When
        /// `false`, the server treats the session as headless for prompt /
        /// bridge purposes and never emits the new clipboard variants.
        /// Defaults to `false` so older clients deserialize cleanly.
        #[serde(default)]
        clipboard_gating: bool,
        /// Feature 013: claim a currently-connected window (takeover). Defaults
        /// to `false` via serde, so every existing local client — which never
        /// sets it — keeps today's behavior exactly: a connected window is
        /// never displaced. `true` is an explicit user action (picker attach or
        /// lost-control reclaim) that atomically swaps the window's writer.
        #[serde(default)]
        takeover: bool,
        /// Explicit local intent to join a connected window's share. Missing
        /// stays false so stale restore claims from older clients cannot join.
        #[serde(default)]
        join_window: bool,
        /// Terminal-image renderer features. Missing means incapable so local
        /// old/new handshakes remain safely decodable.
        #[serde(default)]
        terminal_images: TerminalImageCapabilities,
        /// CI run-bar protocol support. Missing means incapable so a new server
        /// never sends an unknown top-level variant to an older local client.
        #[serde(default)]
        ci_run_bar: bool,
        /// Structured Pi provider support. Missing means the peer must use only
        /// legacy [`ShellTool::Pi`] launch and session metadata.
        #[serde(default)]
        pi_provider: bool,
        /// Agent control-surface protocol support. Missing means this participant
        /// must not receive agent activity or prompt frames.
        #[serde(default)]
        agent_api: bool,
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
    /// Hide CI for this tracked head. The server validates the repo against the
    /// sender's window and synchronizes the clear across capable attached clients.
    DismissCiRun {
        repo_root: PathBuf,
        head_sha: String,
    },
    /// Start or stop fetching job detail for one visible CI head.
    SetCiRunDetailsInterest {
        repo_root: PathBuf,
        head_sha: String,
        interested: bool,
    },
    /// User explicitly requested an update check (e.g. "Check now" button in
    /// settings). The server replies with a single `UpdateCheckResult` on the
    /// same connection. May be sent as the first message on a transient
    /// connection that never registers a window, or after a `Hello` on a
    /// regular client connection.
    CheckForUpdates,
    /// User opened the Releases settings panel. The server replies with a
    /// single `ReleaseList` on the same connection. Mirrors the request shape
    /// of `CheckForUpdates`: no payload, sent over a transient or registered
    /// connection.
    ListReleases,
    /// Request a list of windows known to the server.
    ListWindows,
    /// Ask a connected client window to run an automation action.
    DispatchAction {
        window_id: Option<WindowId>,
        action: AutomationAction,
    },
    /// One-shot request through the local agent control surface.
    AgentRequest(AgentRequest),
    /// User resolution for a pending agent capability prompt.
    AgentPromptResponse {
        prompt_id: PromptId,
        decision: ClipboardDecision,
    },
    /// Completion report for a correlated agent-dispatched action.
    ActionCompleted {
        correlation_id: u64,
        outcome: AgentActionOutcome,
        #[serde(default)]
        created_session_id: Option<SessionId>,
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
    /// Transient one-shot connection from `scribe-hook-helper`. Carries one
    /// `HookEvent` from an AI tool hook subprocess. The server dispatches via
    /// `hook_ingress::handle` and closes the connection — no Welcome, no
    /// window registration, no reply expected.
    HookEvent(hook::HookEvent),
    /// Triggered by the settings UI when the user attempts to enable
    /// `terminal.env_persistence.enabled`. The server replies with exactly one
    /// `ServerMessage::EnvPreflightResult`. No fields.
    EnvPreflight,
    /// Spec 010: user resolution for a pending OSC 52 confirmation overlay.
    /// Echoes the `request_id` from the matching
    /// `ServerMessage::ClipboardPromptRequest`.
    ClipboardPromptResponse {
        request_id: PromptId,
        decision: ClipboardDecision,
    },
    /// Spec 010: reply to `ServerMessage::ClipboardBridgeReadRequest` carrying
    /// the host clipboard payload (or a `BridgeError` if `arboard` failed).
    ClipboardBridgeReadReply {
        request_id: PromptId,
        payload: Result<String, BridgeError>,
    },
    /// Feature 013 remote-only preamble — the FIRST frame a remote (TCP) client
    /// sends. NEVER sent on the local Unix socket. Carries no window or session
    /// data so identity, authorization, and version can all be checked and a
    /// typed refusal issued before any window state is revealed (FR-003).
    RemoteHandshake {
        /// [`REMOTE_PROTOCOL_VERSION`] of the dialer; gated by exact match.
        remote_protocol_version: u32,
        /// Human-readable Scribe version of the dialer, for mismatch copy.
        scribe_version: String,
        /// Dialer's short device name, display-only (banner / audit).
        device_name: String,
    },
    /// Feature 013 local-only request: enumerate the user's own online tailnet
    /// peers for the connect picker. Served from THIS machine's own `LocalAPI`
    /// view; refused (like any non-`RemoteHandshake` first frame) over TCP so a
    /// remote peer cannot enumerate a third machine's tailnet view.
    ListRemotePeers,
    /// Feature 013 local-only settings request: report this machine's remote
    /// environment for the Settings → Remote section — the signed-in tailnet
    /// account name and whether Tailscale is detected at all. The server replies
    /// with exactly one [`ServerMessage::RemoteEnv`]. Like [`ListRemotePeers`]
    /// it is served from THIS machine's own `LocalAPI` view and refused as a
    /// non-`RemoteHandshake` first frame over TCP, so a remote peer can never
    /// read a third machine's tailnet identity. No fields.
    GetRemoteEnv,
    /// Feature 014 LAN preamble — the FIRST frame a LAN (TLS) client sends once
    /// the mutual-TLS handshake completes, before any `Hello`. NEVER sent on the
    /// local Unix socket or the tailnet transport. Identity is the pinned TLS
    /// client certificate (`device_id = SHA-256(SPKI)`), NOT this message; the
    /// `device_name` is the peer's advertised display label only. Carries no
    /// window or session data so the owning side can run the device-approval and
    /// exact-match version gates before any state is revealed (SEC-001,
    /// contracts/lan-protocol.md).
    LanHello {
        /// Peer's advertised display name (banner / approval prompt / audit).
        device_name: String,
        /// [`REMOTE_PROTOCOL_VERSION`] of the dialer; gated by exact match.
        remote_protocol_version: u32,
    },
    /// Feature 014 local-only reply carrying the owning user's decision on a
    /// pending LAN device approval. Sent by the OWNING machine's own local client
    /// in response to a [`ServerMessage::LanApprovalRequest`], echoing its
    /// `request_id`. Refused over any remote transport — the GUI, never the
    /// remote TLS stream, answers the prompt (contracts/lan-protocol.md).
    LanApprovalDecision {
        /// Correlates with the originating [`ServerMessage::LanApprovalRequest`].
        request_id: u64,
        /// `true` writes a trusted device and proceeds; `false` refuses
        /// ([`LanRefusal::Declined`]).
        approve: bool,
    },
    /// Feature 014 local-only request: enumerate LAN peers discovered via mDNS on
    /// the current network for the connect picker. Served from THIS machine's own
    /// discovery view and refused (like any non-`LanHello` / non-`RemoteHandshake`
    /// first frame) over a remote transport. Answered with exactly one
    /// [`ServerMessage::LanPeerList`]. No fields.
    ListLanPeers,
    /// Feature 014 local-only request: list this machine's approved LAN devices
    /// for the Settings → Remote "Local network" section. Answered with exactly
    /// one [`ServerMessage::TrustedDeviceList`]. Local socket only. No fields.
    ListTrustedDevices,
    /// Feature 014 local-only request: revoke a trusted LAN device by its
    /// hex-encoded `device_id` (the `device_id_hex` from [`TrustedDeviceInfo`]).
    /// Removes the pin and severs only that device's live LAN connection, forcing
    /// re-approval on the next attempt (FR-010). Local socket only.
    RevokeTrustedDevice {
        /// Lowercase hex of the 32-byte `device_id = SHA-256(SPKI)`.
        device_id: String,
    },
    /// Feature 014 local-only request: list the user's trusted networks and
    /// whether the current network is among them. Answered with exactly one
    /// [`ServerMessage::TrustedNetworkList`]. Local socket only. No fields.
    ListTrustedNetworks,
    /// Feature 014 local-only request: mark the network the machine is currently
    /// on as trusted (fingerprinted by gateway MAC + subnet). Acked, or errored
    /// when the network cannot be fingerprinted (zero gateway MAC / VPN-only).
    /// Local socket only. No fields.
    AddCurrentNetworkTrusted,
    /// Feature 014 local-only request: remove a trusted network by its record
    /// `id` (the `id` from [`TrustedNetworkInfo`]). Removing the current network
    /// makes the LAN surface go dormant. Local socket only.
    RemoveTrustedNetwork {
        /// The [`TrustedNetworkInfo`] record id to remove.
        id: String,
    },
    /// Feature 014 local-only settings request: report this machine's LAN
    /// environment for the Settings → Remote "Local network" section — this
    /// device's OWN identity fingerprint (word list + hex) for the optional
    /// out-of-band MITM compare (FR-006), and whether the CURRENT network can be
    /// fingerprinted as a trusted network (drives the "Add current network"
    /// control's enabled/disabled state and explanatory note). Served from THIS
    /// machine's own view and refused as a non-`LanHello` / non-`RemoteHandshake`
    /// first frame over any remote transport, exactly like [`GetRemoteEnv`], so a
    /// remote peer can never read a third machine's identity. Answered with
    /// exactly one [`ServerMessage::LanEnv`]. No fields.
    GetLanEnv,
    /// Feature 014 (LAN dial-identity fix) local-only request: hand this
    /// machine's OWN device identity (public certificate DER + sealed `PKCS#8`
    /// private-key DER) to a co-located connecting `scribe-client` so the dialer
    /// can build its mutual-TLS identity WITHOUT reading the OS keyring from a
    /// different binary. On macOS the sealed device key's legacy `SecKeychain`
    /// per-item ACL trusts ONLY the creating binary (`scribe-server`), so a
    /// cross-binary read is denied (errSecInteractionNotAllowed) with no usable
    /// prompt; the server therefore stays the SOLE keychain accessor and serves the
    /// identity here. Refused as a non-`Hello` first frame over any remote transport
    /// (exactly like [`GetLanEnv`]), so a remote peer can never exfiltrate this
    /// machine's private device key. Answered with exactly one
    /// [`ServerMessage::LanDialIdentity`]. No fields.
    GetLanDialIdentity,
    /// Feature 015: a participant takes input control of a shared window in
    /// [`SharingMode::SharedSingleTypist`] mode under
    /// `control_acquisition = FreeClaim` (default), or the owning machine claims
    /// regardless of acquisition setting (FR-005, FR-007). The server sets the
    /// holder, demotes the previous holder to a still-live viewer, and
    /// broadcasts a fresh [`ServerMessage::ShareRoster`]. No-op in `FreeForAll`;
    /// not applicable in `SingleController`. Additive v3 variant, never sent by a
    /// v2 client or on the local Unix socket.
    ControlClaim {
        window_id: WindowId,
    },
    /// Feature 015: a viewer asks for input control under
    /// `control_acquisition = RequestAndGrant`. The server records the pending
    /// request and sends [`ServerMessage::ControlRequested`] to the current
    /// holder (or the owner if unheld). Cancelled on holder change or mode
    /// change. Additive v3 variant.
    ControlRequest {
        window_id: WindowId,
    },
    /// Feature 015: the current holder (or owner) answers a pending
    /// [`ControlRequest`]. On `accept = true` the server transfers the holder to
    /// `participant_id` and broadcasts [`ServerMessage::ShareRoster`]; on `false`
    /// it clears the pending request and sends [`ServerMessage::ControlDenied`]
    /// to the requester. Only honored from the approver named by the request
    /// (FR-005). Additive v3 variant.
    ControlGrant {
        window_id: WindowId,
        /// Server-monotonic id of the grant target (the requester).
        participant_id: u64,
        /// `true` transfers control; `false` denies the request.
        accept: bool,
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
    /// Full per-cell screen state, sent in response to `RequestSnapshot`.
    /// Used by tooling (`scribe-cli`, `scribe-test`) for JSON dumps and
    /// visual diffs. The reattach path uses `SessionReplay` instead.
    ScreenSnapshot {
        session_id: SessionId,
        snapshot: crate::screen::ScreenSnapshot,
    },
    /// zstd-compressed ANSI replay of a session's visible grid + scrollback.
    /// Sent after `AttachSessions` in place of the legacy per-cell
    /// `ScreenSnapshot`. The client decompresses the bytes and feeds them
    /// through its VTE processor to rebuild the pane's `Term` durably.
    SessionReplay {
        session_id: SessionId,
        replay: crate::screen_replay::SessionReplay,
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
    IconTitleChanged {
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
    TaskLabelChanged {
        session_id: SessionId,
        provider: AiProvider,
        task_label: String,
    },
    TaskLabelCleared {
        session_id: SessionId,
        provider: AiProvider,
    },
    /// A user prompt was submitted in a supported AI coding session.
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
    /// One bounded CI state replacement or clear for workspaces rooted in `repo_root`.
    CiRunState {
        repo_root: PathBuf,
        delta: CiRunDelta,
    },
    /// Expanded job detail for an interested client and visible head.
    CiRunDetails {
        repo_root: PathBuf,
        details: CiRunDetails,
    },
    SessionCreated {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        /// Basename of the shell binary (e.g. "zsh", "bash").
        shell_name: String,
    },
    SessionExited {
        session_id: SessionId,
        /// Exit status of a child that terminated normally. `None` when the
        /// child was signalled, when the session was closed explicitly, and
        /// for handoff-inherited sessions, whose child predates this server
        /// process and carries no wait status it can observe.
        exit_code: Option<i32>,
        /// Signal number that terminated the child, when it died on a signal.
        /// Kept separate from `exit_code` rather than folded into it as a
        /// negative value, so a status and a signal are never confusable.
        /// Additive: older senders omit the key.
        #[serde(default)]
        signal: Option<i32>,
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
        /// Per-workspace metadata (name, accent color, split direction, project
        /// root) for every workspace referenced by `sessions`. Batched here so
        /// the reattach flow does not need a per-session `WorkspaceInfo`
        /// fan-out.
        #[serde(default)]
        workspaces: Vec<WorkspaceListEntry>,
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
    /// Cached Beads-board state for `RequestBeadsBoard`.
    BeadsBoard {
        workspace_id: WorkspaceId,
        protocol_version: u32,
        state: BeadsBoardState,
    },
    /// Complete issue data, or `None` when the requested issue vanished.
    BeadsIssueDetail {
        workspace_id: WorkspaceId,
        issue_id: String,
        detail: Option<Box<BeadsIssueDetail>>,
    },
    /// Correlated reply for `RequestBeadsEpicGraph`. The workspace and epic ids
    /// are repeated so a client can discard a reply it no longer wants.
    BeadsEpicGraph {
        workspace_id: WorkspaceId,
        epic_id: String,
        outcome: BeadsEpicGraphOutcome,
    },
    /// Current Beads issue a live agent session is working on. `None` clears
    /// the binding when the agent leaves, its session ends, or its client
    /// disconnects.
    IssueFocused {
        session_id: SessionId,
        issue_id: Option<String>,
    },
    /// Correlated outcome for one typed issue write.
    BeadsIssueWriteResult {
        workspace_id: WorkspaceId,
        issue_id: String,
        result: BeadsIssueWriteResult,
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
        /// Spec 010: server advertises OSC 52 clipboard-gating support. When
        /// `false`, the client should not expect the new clipboard variants
        /// and should keep its legacy user-driven paste path. Defaults to
        /// `false` so older servers deserialize cleanly.
        #[serde(default)]
        clipboard_gating: bool,
        /// Feature 015: this connection's own server-assigned `participant_id`
        /// in the window's share, so a remote client can match itself in a
        /// [`ServerMessage::ShareRoster`] exactly (e.g. its own `is_holder`)
        /// instead of comparing device names. `None` from an older server or
        /// when the claim did not register a participant (a lost-control
        /// landing). Additive, `#[serde(default)]` so local / legacy flows are
        /// unaffected (contracts/remote-protocol-v3.md).
        #[serde(default)]
        participant_id: Option<u64>,
        /// Effective terminal-image subset for this connection. Missing from an
        /// older local server means unsupported.
        #[serde(default)]
        terminal_images: TerminalImageCapabilities,
        /// Server support for the additive Beads issue-detail messages.
        /// Missing from an older local server means the board remains read-only.
        #[serde(default)]
        beads_detail: bool,
        /// Server support for typed Beads issue writes. This stays independent
        /// from detail reads because bd 1.1.0 cannot enforce write guards.
        #[serde(default)]
        beads_write: bool,
        /// Server support for the additive epic-graph request behind the Flow
        /// view. Independent from detail reads and writes: a server may serve
        /// cards without serving graphs. Missing from an older local server
        /// means the client never leaves the Lanes rendering.
        #[serde(default)]
        beads_flow: bool,
        /// Structured Pi provider support. Missing means the client must keep
        /// using legacy [`ShellTool::Pi`] metadata.
        #[serde(default)]
        pi_provider: bool,
        /// Agent control-surface protocol support negotiated with the client.
        /// Missing from older servers means unsupported.
        #[serde(default)]
        agent_api: bool,
    },
    /// One bounded live terminal-image record at an output boundary.
    TerminalImageLive {
        session_id: SessionId,
        message: TerminalImageLiveMessage,
    },
    /// One bounded generation-tagged image replay record.
    TerminalImageReplay {
        session_id: SessionId,
        message: TerminalImageReplayMessage,
    },
    /// Typed refusal for an incapable viewer of an image-enabled session.
    TerminalImageCapabilityMismatch {
        session_id: SessionId,
        mismatch: TerminalImageCapabilityMismatch,
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
    /// Correlated automation request originating from the agent API.
    RunActionCorrelated {
        correlation_id: u64,
        action: AutomationAction,
    },
    /// Confirms that a requested automation action was routed to a target window.
    ActionDispatched {
        window_id: WindowId,
    },
    /// Reply to a one-shot agent control-surface request.
    AgentResponse(AgentResponse),
    /// Prompt a capable client to approve an agent capability request.
    AgentPromptRequest {
        prompt_id: PromptId,
        agent_label: String,
        capability: AgentCapability,
        target: String,
    },
    /// Shows agent activity for a session to capable participants.
    AgentActivity {
        session_id: SessionId,
        active: bool,
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
    /// Result of a manual `CheckForUpdates` request, sent back over the
    /// requesting connection. When an update is available the server also
    /// broadcasts the usual `UpdateAvailable` to every connected client.
    UpdateCheckResult {
        state: UpdateCheckResultState,
    },
    /// Result of a `ListReleases` request, sent back over the requesting
    /// connection. Mirrors the shape of `UpdateCheckResult`: a single struct
    /// variant that wraps the result-state enum so the wire format follows
    /// the existing pattern.
    ReleaseList {
        state: ReleaseListResultState,
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
    /// The server trimmed AI-added redraw history after suppressing ED 3.
    /// The client should shrink its scrollback to `history_rows` before
    /// applying subsequent PTY bytes so committed transcript history stays intact
    /// without accumulating duplicate full-screen redraw frames.
    TrimScrollback {
        session_id: SessionId,
        history_rows: u32,
    },
    /// Legacy bottom-snap frame retained for named `MessagePack` compatibility.
    /// Clients preserve a nonzero `display_offset` so an old server cannot
    /// override a viewport the user is reading.
    ScrollBottom {
        session_id: SessionId,
    },
    /// Reply to `ClientMessage::EnvPreflight`. `ok = true` ⇒ the OS secret
    /// store is reachable and usable for our identifier; the settings layer
    /// commits the toggle. `ok = false` ⇒ `error` carries the actionable
    /// reason for the UI.
    EnvPreflightResult {
        ok: bool,
        #[serde(default)]
        error: Option<PreflightError>,
    },
    /// Per-session runtime status for env-capture. Sent on transitions only,
    /// not periodically. Drives the status-bar warning glyph in the client.
    EnvStatus {
        session_id: SessionId,
        state: EnvStatusState,
    },
    /// Spec 010: server requests user confirmation for an OSC 52 read or write
    /// originating from `session_id`. The client renders a clipboard prompt
    /// dialog and replies with `ClientMessage::ClipboardPromptResponse`
    /// carrying the same `request_id`. `preview` carries a head-and-tail
    /// truncated write payload preview per FR-006; always `None` for reads.
    ClipboardPromptRequest {
        session_id: SessionId,
        request_id: PromptId,
        op: ClipboardOp,
        selection: ClipboardSelection,
        #[serde(default)]
        preview: Option<String>,
    },
    /// Spec 010: server forwards an allowed OSC 52 write to the client's host
    /// clipboard bridge. No reply expected (OSC 52 has no write-ack).
    ClipboardBridgeWrite {
        session_id: SessionId,
        selection: ClipboardSelection,
        payload: String,
    },
    /// Spec 010: server requests the client to read the host clipboard for an
    /// allowed OSC 52 read. The client replies with
    /// `ClientMessage::ClipboardBridgeReadReply` carrying the matching
    /// `request_id`.
    ClipboardBridgeReadRequest {
        session_id: SessionId,
        request_id: PromptId,
        selection: ClipboardSelection,
    },
    /// Feature 013 reply to [`ClientMessage::RemoteHandshake`]. Always sent
    /// after the preamble is read so every refusal reaches the dialer as a
    /// typed outcome (UX-002). On `accepted = false` the server closes the
    /// connection after sending this frame.
    RemoteHandshakeReply {
        accepted: bool,
        /// Present iff `!accepted`; names the typed refusal reason.
        #[serde(default)]
        refusal: Option<RemoteRefusal>,
        /// Server's [`REMOTE_PROTOCOL_VERSION`], named for mismatch copy.
        server_remote_protocol_version: u32,
        /// Server's human-readable Scribe version, named for mismatch copy.
        server_scribe_version: String,
        /// Present for `IncompatibleVersion`; old remote peers are already
        /// refused by exact version before this additive field matters.
        #[serde(default)]
        version_mismatch: Option<RemoteProtocolMismatch>,
    },
    /// Feature 013: sent to a client whose window was claimed by another
    /// controller. The displaced client stops sending input for that window,
    /// freezes and dims its last frame under a banner naming the new
    /// controller, and offers one-action reclaim.
    WindowTakenOver {
        /// New controller's device name (or "this machine" for a local reclaim).
        device_name: String,
        /// New controller's account display name.
        login_name: String,
    },
    /// Feature 013 best-effort final frame before the server closes a remote
    /// connection for a policy reason (v1: remote access disabled). The close
    /// follows regardless of whether this frame is delivered.
    RemoteDisconnect {
        reason: RemoteRefusal,
    },
    /// Feature 013 reply to [`ClientMessage::ListRemotePeers`] — this machine's
    /// same-account tailnet peers for the connect picker. Local socket only.
    RemotePeerList {
        peers: Vec<RemotePeerInfo>,
    },
    /// Feature 013 reply to [`ClientMessage::GetRemoteEnv`] — this machine's
    /// remote environment for the Settings → Remote section (UX-003, FR-015).
    /// `account` is the signed-in tailnet login name, absent when unknown;
    /// `tailscale_detected` is `false` when the `LocalAPI` could not be reached
    /// at all, which drives the passive "Tailscale not detected" notice. Any
    /// `LocalAPI` error fails closed to `{ account: None, tailscale_detected:
    /// false }`. Local socket only.
    RemoteEnv {
        #[serde(default)]
        account: Option<String>,
        tailscale_detected: bool,
    },
    /// Feature 014: owning → connecting notice that the LAN connection is held
    /// pending device approval on the owning machine. MUST be sent before any
    /// window data so the connecting client can show a "waiting for approval on
    /// <peer>" state (FR-014, US2.5). No window or session data flows until a
    /// [`ServerMessage::LanApprovalResult`] with `approved = true`
    /// (contracts/lan-protocol.md). No fields.
    LanApprovalPending,
    /// Feature 014: owning → connecting terminal outcome of the LAN approval
    /// gate. `approved = true` means proceed to `Hello`; `approved = false` means
    /// refused, with `refusal` naming the typed [`LanRefusal`] reason (present
    /// iff `!approved`). On refusal the server closes the connection after this
    /// frame (contracts/lan-protocol.md).
    LanApprovalResult {
        approved: bool,
        /// Present iff `!approved`; names the typed refusal reason.
        #[serde(default)]
        refusal: Option<LanRefusal>,
    },
    /// Feature 014: owning server → its OWN local client — an unknown LAN device
    /// has completed the mutual-TLS handshake and is pending the user's approval.
    /// Carries the peer's advertised name, identity fingerprint words, and the
    /// trusted network it arrived on for the approval prompt; `name_collision` is
    /// an informational hint (never a trust key) that an already-trusted device
    /// shares this advertised name. Answered with
    /// [`ClientMessage::LanApprovalDecision`] carrying the same `request_id`.
    /// Local socket only — never sent over a remote transport.
    LanApprovalRequest {
        /// Correlates this push with the [`ClientMessage::LanApprovalDecision`].
        request_id: u64,
        /// Requesting device's advertised name (display only).
        device_name: String,
        /// The peer's identity fingerprint words (research D8).
        fingerprint_words: String,
        /// The trusted network the request arrived on.
        network_label: String,
        /// `true` when an already-trusted device shares this advertised name
        /// (informational hint only).
        name_collision: bool,
    },
    /// Feature 014 reply to [`ClientMessage::ListLanPeers`] — LAN peers discovered
    /// via mDNS on the current network, for the connect picker. Local socket only.
    LanPeerList {
        peers: Vec<LanPeerInfo>,
    },
    /// Feature 014 reply to [`ClientMessage::ListTrustedDevices`] — this machine's
    /// approved LAN devices for the Settings "Local network" section. Local
    /// socket only.
    TrustedDeviceList {
        devices: Vec<TrustedDeviceInfo>,
    },
    /// Feature 014 reply to [`ClientMessage::ListTrustedNetworks`] — the user's
    /// trusted networks and whether the current network is one of them
    /// (`current_trusted` drives the active/dormant status line, UX-004). Local
    /// socket only.
    TrustedNetworkList {
        networks: Vec<TrustedNetworkInfo>,
        current_trusted: bool,
    },
    /// Feature 014 reply to [`ClientMessage::GetLanEnv`] — this machine's own LAN
    /// environment for the Settings "Local network" section. `device_id_hex`
    /// (lowercase hex of `device_id = SHA-256(SPKI)`) and `fingerprint_words`
    /// are this machine's OWN identity, both absent until the identity has been
    /// generated (first LAN enable). `current_network_addable` is `true` when the
    /// network the machine is currently on can be fingerprinted as a trusted
    /// network (non-zero gateway MAC, physical LAN, not VPN-only); when `false`,
    /// `current_network_reason` carries the short note for the disabled "Add
    /// current network" control. Any local error fails closed to identity `None`
    /// with `current_network_addable = false`. Local socket only.
    LanEnv {
        #[serde(default)]
        device_id_hex: Option<String>,
        #[serde(default)]
        fingerprint_words: Option<String>,
        current_network_addable: bool,
        #[serde(default)]
        current_network_reason: Option<String>,
    },
    /// Feature 014 (LAN dial-identity fix) reply to
    /// [`ClientMessage::GetLanDialIdentity`] — this machine's OWN device identity
    /// for a co-located connecting `scribe-client` to build its mutual-TLS dialer
    /// without touching the OS keyring. `available` is `true` only when the server
    /// resolved (minting on first use, like the owning side) a usable identity; in
    /// that case `cert_der` is the public certificate DER and `private_key_pkcs8_der`
    /// is the sealed `PKCS#8` private-key DER. On any keyring/state-dir error
    /// `available` is `false` and both byte fields are empty, so the client fails
    /// closed and never dials without an identity. `private_key_pkcs8_der` is PRIVATE
    /// key material: this message is local-socket only (never crosses a remote
    /// transport) and is never logged.
    LanDialIdentity {
        available: bool,
        cert_der: Vec<u8>,
        private_key_pkcs8_der: Vec<u8>,
    },
    /// Feature 015: full-state roster broadcast to every participant of a shared
    /// window on every join, leave, control transfer, ejection, and mode change
    /// (FR-008, D8). Never a delta — always the complete current roster
    /// (contracts/remote-protocol-v3.md). Additive v3 message.
    ShareRoster {
        window_id: WindowId,
        /// The complete current participant list.
        participants: Vec<ParticipantInfo>,
        /// The window's active sharing mode.
        mode: SharingMode,
        /// The current input-control holder's participant id, or `None` when
        /// unheld or not applicable to the mode.
        #[serde(default)]
        holder: Option<u64>,
    },
    /// Feature 015: sent to the current holder (or the owner if unheld) when a
    /// viewer requests input control under `RequestAndGrant`. The recipient
    /// answers with [`ClientMessage::ControlGrant`]. Additive v3 message.
    ControlRequested {
        window_id: WindowId,
        /// The requesting participant.
        from: ParticipantInfo,
    },
    /// Feature 015: sent to a requester when a [`ClientMessage::ControlGrant`]
    /// with `accept = false` denies the request, or the pending request was
    /// cancelled by a holder change or mode change. Additive v3 message.
    ControlDenied {
        window_id: WindowId,
    },
    /// Feature 015: sent to every remote participant of a share when the owning
    /// machine closes the window/session or flips to `SingleController`
    /// (FR-017). For a `SingleController` flip, remote participants also receive
    /// the legacy [`ServerMessage::WindowTakenOver`] for the frozen displaced UI;
    /// `ShareEnded` is the mode-neutral roster/notice signal. Additive v3
    /// message.
    ShareEnded {
        window_id: WindowId,
        reason: ShareEndReason,
    },
}

/// Feature 015: why a shared window's session ended for its remote participants
/// (contracts/remote-protocol-v3.md, [`ServerMessage::ShareEnded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareEndReason {
    /// The owning machine closed the window.
    OwnerClosed,
    /// The shared session itself closed.
    WindowClosed,
    /// The owner flipped the sharing mode to `SingleController`, ending the
    /// share for all remote participants.
    ModeChangedToSingleController,
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
/// Carries everything the server needs to persist and relay so the client can
/// reconstruct its `WindowNode` tree exactly on reconnect: split topology,
/// split ratios, per-workspace tab ordering, per-tab pane layouts, and the
/// per-workspace active tab index.
///
/// Accent colours and names still travel in `WorkspaceInfo` / `WorkspaceListEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// Index into `session_ids` of the tab that should be active on
        /// reconnect/handoff. Populated by client when reporting tree;
        /// defaults to 0 when received from older peers that don't ship it
        /// (e.g. mid-upgrade handoff envelope from a pre-active-tab server).
        #[serde(default)]
        active_tab_index: usize,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Per-workspace metadata carried in `SessionList` responses.
///
/// Replaces the per-session `WorkspaceInfo` fan-out that used to follow every
/// `AttachSessions` reply. Clients apply one entry per workspace up front so
/// the attach pipeline can ship session replays in parallel without waiting
/// for redundant metadata messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceListEntry {
    pub workspace_id: WorkspaceId,
    pub name: Option<String>,
    /// Hex color string (e.g. "#a78bfa") from the rotating accent palette.
    pub accent_color: String,
    /// Direction of the split that created this workspace. `None` for the
    /// initial (unsplit) workspace.
    pub split_direction: Option<LayoutDirection>,
    /// Absolute path to the project directory (root + first CWD component).
    #[serde(default)]
    pub project_root: Option<PathBuf>,
}

/// Prompt-bar history the server retains for a session, so a client that
/// attaches later can paint the bar immediately instead of waiting for the
/// next `PromptReceived` hook.
///
/// Mirrors the client's `PromptBarData` minus its purely local `dismissed`
/// flag. Instants travel as Unix-epoch seconds rather than `SystemTime`,
/// matching how the restore snapshot already persists them.
/// Every field carries `#[serde(default)]` so the record can be `#[serde(flatten)]`ed
/// into the client's persisted launch record, where snapshots written before a
/// field existed still have to load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPromptState {
    /// Number of prompts submitted in the current conversation.
    #[serde(default)]
    pub prompt_count: u32,
    /// Text of the first prompt in the conversation.
    #[serde(default)]
    pub first_prompt: Option<String>,
    /// Text of the most recent prompt.
    #[serde(default)]
    pub latest_prompt: Option<String>,
    /// When the latest prompt was submitted (the bar's timer origin).
    #[serde(default)]
    pub latest_prompt_at: Option<u64>,
    /// When the AI last finished, freezing the bar's elapsed timer. Stamped
    /// on the first state edge that leaves `Processing`, so a reattaching
    /// client reads back the frozen figure rather than a timer that starts
    /// running again from the original prompt instant.
    #[serde(default)]
    pub latest_prompt_finished_at: Option<u64>,
}

impl SessionPromptState {
    /// Fold one submitted prompt onto the retained history: the first prompt is
    /// latched once, the latest one is replaced, and the elapsed timer restarts.
    ///
    /// Shared by the server's `PromptReceived` handler and the client's
    /// `AiChrome`, so a bar painted from a `SessionList` is the same bar the
    /// live hook path would have built.
    pub fn record_prompt(&mut self, text: &str, at: SystemTime) {
        self.prompt_count = self.prompt_count.saturating_add(1);
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.to_owned());
        }
        self.latest_prompt = Some(text.to_owned());
        self.latest_prompt_at = epoch_secs(at);
        self.latest_prompt_finished_at = None;
    }

    /// Freeze or resume the elapsed prompt timer for one AI state edge.
    ///
    /// Leaving `Processing` stamps the instant the timer freezes at, so the
    /// figure reads prompt-to-finish rather than wall-clock-since-prompt; a
    /// return to `Processing` clears the stamp and the timer ticks again. The
    /// stamp is taken once per run rather than on every non-`Processing` edge,
    /// because an idle provider keeps emitting them and each one would push a
    /// frozen value forward.
    pub fn note_prompt_progress(&mut self, state: &AiState, at: SystemTime) {
        if matches!(state, AiState::Processing) {
            self.latest_prompt_finished_at = None;
        } else if self.latest_prompt_finished_at.is_none() {
            self.latest_prompt_finished_at = epoch_secs(at);
        }
    }
}

/// Unix-epoch seconds for a wall-clock instant — the wire and on-disk form both
/// prompt timestamps travel in.
///
/// A clock set before 1970 encodes as no timestamp at all, which is exactly how
/// a record written before the field existed reads back.
#[must_use]
pub fn epoch_secs(at: SystemTime) -> Option<u64> {
    at.duration_since(SystemTime::UNIX_EPOCH).ok().map(|since| since.as_secs())
}

/// Inverse of [`epoch_secs`], used wherever a stored stamp is compared against
/// a live `SystemTime`.
#[must_use]
pub fn from_epoch_secs(secs: Option<u64>) -> Option<SystemTime> {
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs?))
}

/// Summary of a live session, sent in `SessionList` responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    /// Stable launch/environment-envelope identity for local client restore.
    /// Omitted for remote clients and payloads from older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<String>,
    /// Basename of the session shell or command entrypoint.
    pub shell_name: String,
    /// Last-known terminal title (from OSC 0/2). `None` before first title event.
    pub title: Option<String>,
    /// Last-known icon/tab title (from OSC 0/1). `None` before first title event.
    #[serde(default)]
    pub icon_title: Option<String>,
    /// Last-known shell/session context (remote host, tmux session).
    #[serde(default)]
    pub context: Option<SessionContext>,
    /// Last-known provider task label. `None` when the session is not showing one.
    #[serde(default)]
    pub task_label: Option<String>,
    /// Legacy Codex task label field retained for backward compatibility.
    #[serde(default)]
    pub codex_task_label: Option<String>,
    /// Last-known working directory (from OSC 7). `None` before first CWD event.
    pub cwd: Option<PathBuf>,
    /// Current git branch detected for `cwd`. `None` when not in a git repo or
    /// when `cwd` is unset. Populated once by the server in `SessionList` so
    /// clients avoid re-deriving it from the CWD on every reattach.
    #[serde(default)]
    pub git_branch: Option<String>,
    /// Last-known AI process state (from OSC 1337). `None` when no AI is active.
    #[serde(default)]
    pub ai_state: Option<AiProcessState>,
    /// Last-known AI provider for the session even when there is no active
    /// visible AI state. Used to preserve provider-aware client behavior on
    /// reconnect after an attention state was dismissed locally.
    #[serde(default)]
    pub ai_provider_hint: Option<AiProvider>,
    /// Launch-only shell tool retained so warm reattach preserves cold-restart
    /// identity. AI metadata remains authoritative when both are present.
    #[serde(default)]
    pub shell_tool: Option<ShellTool>,
    /// Prompt history retained for the session. `None` when the session has
    /// submitted no prompt, and absent entirely on payloads from servers that
    /// predate the field.
    #[serde(default)]
    pub prompt_state: Option<SessionPromptState>,
}

impl SessionInfo {
    /// Replace structured Pi metadata with the legacy shell-tool shape when the
    /// receiving local peer did not advertise Pi provider support.
    pub fn make_pi_provider_compatible(&mut self, pi_provider: bool) {
        if pi_provider
            || (self.ai_state.as_ref().map(|state| state.provider) != Some(AiProvider::Pi)
                && self.ai_provider_hint != Some(AiProvider::Pi))
        {
            return;
        }
        self.ai_state = None;
        self.ai_provider_hint = None;
        self.shell_tool = Some(ShellTool::Pi);
        self.task_label = None;
        self.codex_task_label = None;
        self.prompt_state = None;
    }
}

impl ServerMessage {
    /// Downgrade this frame for a peer that cannot deserialize
    /// [`AiProvider::Pi`]. Returns `false` when the frame must be withheld.
    pub fn make_pi_provider_compatible(&mut self, pi_provider: bool) -> bool {
        if pi_provider {
            return true;
        }
        match self {
            ServerMessage::AiStateChanged { ai_state, .. } => ai_state.provider != AiProvider::Pi,
            ServerMessage::TaskLabelChanged { provider, .. }
            | ServerMessage::TaskLabelCleared { provider, .. }
            | ServerMessage::PromptReceived { provider, .. } => *provider != AiProvider::Pi,
            ServerMessage::SessionList { sessions, .. } => {
                for session in sessions {
                    session.make_pi_provider_compatible(false);
                }
                true
            }
            _ => true,
        }
    }
}

/// A single search match location in the terminal grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub row: i32,
    pub col_start: u16,
    pub col_end: u16,
}

/// Outcome of a manual update check requested via `CheckForUpdates`.
///
/// Mirrors what the periodic checker logs internally but is shaped for direct
/// user-facing feedback (e.g. "Up to date", "v1.2.3 available", "Check failed").
/// `UpdateAvailable` here always follows a fresh broadcast of the same version,
/// even when the version was previously dismissed — manual checks override
/// dismissal so the user always sees the up-to-date state of the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateCheckResultState {
    /// The current build is the latest released version on the active channel.
    NoUpdate,
    /// A newer version is available; the same data was also broadcast as
    /// `ServerMessage::UpdateAvailable` to every connected client.
    UpdateAvailable { version: String, release_url: String },
    /// The check failed (network error, GitHub API failure, etc.).
    Failed { reason: String },
}

/// One published Scribe release, in the post-render shape the settings webview
/// can display directly.
///
/// Built on the server side from a GitHub releases response: `tag_name` becomes
/// `version` (with the leading `v` stripped, mirroring the existing updater
/// convention), and the markdown `body` is run through `pulldown-cmark` and
/// `ammonia::clean` to produce the sanitized `body_html`. The settings binary
/// receives `Release` values verbatim and renders them as static HTML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Release {
    /// `tag_name` from GitHub with the leading `v` stripped (e.g. "0.4.2").
    pub version: String,
    /// Optional human title from GitHub's `name` field. May equal `version`.
    pub name: Option<String>,
    /// ISO-8601 timestamp from GitHub's `published_at`, kept verbatim.
    pub published_at: String,
    /// Sanitized HTML, ready to be assigned to `innerHTML` in the webview.
    pub body_html: String,
    /// `true` when GitHub marks the release as a pre-release.
    pub prerelease: bool,
    /// Canonical GitHub URL for the "View on GitHub" panel-header link.
    pub html_url: String,
}

/// Outcome of a `ListReleases` request, parallel in structure to
/// [`UpdateCheckResultState`].
///
/// The `Stale` variant carries the cached release vector alongside a reason
/// string so the panel can render the cached data with a "may be stale"
/// indicator instead of dropping back to a pure error state — see FR-013 in
/// the feature spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReleaseListResultState {
    /// Cache was within TTL, or the synchronous on-demand fetch just succeeded.
    Fresh { releases: Vec<Release> },
    /// Cache existed but was past TTL; a background refresh either failed or
    /// has not completed. The webview renders `releases` plus a stale banner
    /// citing `reason`.
    Stale { releases: Vec<Release>, reason: String },
    /// No cache exists and the on-demand fetch failed.
    Failed { reason: String },
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

/// Reason the env-persistence preflight failed. Reported back to the settings
/// UI inside `ServerMessage::EnvPreflightResult` so the toggle can surface an
/// actionable message instead of committing a setting the keystore can't back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PreflightError {
    /// macOS: login keychain is locked.
    KeychainLocked,
    /// Linux: D-Bus session bus or Secret Service not available.
    SecretServiceUnavailable,
    /// Either platform: keystore access denied for our identifier.
    KeystoreAccessDenied,
    /// Any other underlying error; `reason` is for diagnostics.
    ///
    /// A struct variant, not a newtype: `#[serde(tag = "type")]` is internal
    /// tagging, and a newtype variant wrapping a `String` cannot be serialized
    /// under it at all — every `EnvPreflightResult` carrying this variant failed
    /// to encode and was dropped before it reached the client.
    Unknown { reason: String },
}

/// Runtime state of env-capture for a single session, carried in
/// `ServerMessage::EnvStatus`. Sent only on transitions so the client can
/// drive its status-bar warning glyph without polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvStatusState {
    /// Env capture is healthy.
    Active,
    /// Keystore became unavailable; persistence has stopped. The on-disk
    /// envelope (if any) is untouched. No plaintext fallback.
    Degraded { reason: String },
}

/// Feature 013: canonical remote-refusal taxonomy. One enum shared by the wire
/// ([`ServerMessage::RemoteHandshakeReply`] `refusal`,
/// [`ServerMessage::RemoteDisconnect`] `reason`) and the server audit log, so
/// refusal reasons never drift between the two surfaces. Each variant maps 1:1
/// to the distinct UX-002 failure copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRefusal {
    /// Remote access is off (races with a live disable).
    Disabled,
    /// Wrong tailnet account, a tagged/identity-less node, or unknown identity.
    Unauthorized,
    /// tailscaled / `WhoIs` unavailable — fail closed (FR-015).
    IdentityUnavailable,
    /// Exact [`REMOTE_PROTOCOL_VERSION`] match failed (both versions in reply).
    IncompatibleVersion,
    /// The remote-connection cap (8) is reached.
    Busy,
}

/// Feature 013: one same-account tailnet peer for the connect picker, resolved
/// from THIS machine's own `LocalAPI` status and carried in
/// [`ServerMessage::RemotePeerList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePeerInfo {
    /// `MagicDNS` short name; shown in the picker and usable as a dial target.
    pub name: String,
    /// Tailnet address to dial (literal IP or MagicDNS-resolvable host).
    pub addr: String,
    /// Whether the peer is currently reachable; offline peers are greyed/omitted.
    pub online: bool,
}

/// Feature 014: canonical LAN-refusal taxonomy, mirroring 013's
/// [`RemoteRefusal`] for the LAN transport. Carried in
/// [`ServerMessage::LanApprovalResult`] `refusal` and the server audit log so
/// refusal reasons never drift between the wire and the log. Each variant maps
/// 1:1 to the distinct failure copy (contracts/settings-and-config.md). There is
/// deliberately NO `IdentityChanged` variant: trust is keyed by
/// `device_id = SHA-256(SPKI)`, so a reinstalled/rekeyed peer presents a new,
/// unpinned `device_id` and is simply an unknown device requiring fresh approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanRefusal {
    /// The owning user declined the approval prompt (or it timed out).
    Declined,
    /// The owning machine raced leaving a trusted network (now dormant).
    NotTrustedNetwork,
    /// LAN access was turned off mid-handshake.
    Disabled,
    /// Exact [`REMOTE_PROTOCOL_VERSION`] match failed (both versions named).
    IncompatibleVersion,
    /// The LAN connection cap was reached.
    Busy,
}

/// Feature 014: one LAN peer discovered via mDNS on the current network,
/// resolved from THIS machine's own discovery view and carried in
/// [`ServerMessage::LanPeerList`] for the connect picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanPeerInfo {
    /// mDNS instance / display name shown in the picker.
    pub name: String,
    /// Peer machine hostname (mDNS TXT `host`); the name-match key for
    /// LAN↔tailnet dedup against the tailnet `MagicDNS` name.
    pub host: String,
    /// Resolved LAN address to dial (filtered to the current subnet).
    pub addr: String,
    /// Control port from the SRV record.
    pub port: u16,
    /// [`REMOTE_PROTOCOL_VERSION`] from TXT `protovers`; the picker keeps an
    /// incompatible peer visible with update guidance but prevents connecting.
    pub protovers: u32,
    /// Whether the peer is currently advertised; evicted peers are greyed/omitted.
    pub online: bool,
}

/// Feature 014: one approved LAN device for the Settings "Local network"
/// trusted-devices list, carried in [`ServerMessage::TrustedDeviceList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedDeviceInfo {
    /// Lowercase hex of the 32-byte `device_id = SHA-256(SPKI)`; the revoke key
    /// echoed back in [`ClientMessage::RevokeTrustedDevice`].
    pub device_id_hex: String,
    /// Human label (the peer's advertised name at approval time).
    pub label: String,
    /// Word-list fingerprint for the list display (research D8).
    pub fingerprint_words: String,
    /// Approval time, Unix epoch milliseconds.
    pub approved_at: u64,
}

/// Feature 014: one trusted network for the Settings "Local network"
/// trusted-networks list, carried in [`ServerMessage::TrustedNetworkList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedNetworkInfo {
    /// Record id; the removal key echoed back in
    /// [`ClientMessage::RemoveTrustedNetwork`].
    pub id: String,
    /// User-facing label (SSID when known, else a gateway-derived default).
    pub label: String,
    /// Normalized lowercase default-gateway MAC (the primary match anchor).
    pub gateway_mac: String,
    /// Local subnet in CIDR form (e.g. `192.168.1.0/24`; secondary corroborator).
    pub subnet_cidr: String,
    /// SSID display hint; `None` on wired links or where the OS withholds it.
    #[serde(default)]
    pub ssid: Option<String>,
    /// When the network was trusted, Unix epoch milliseconds.
    pub added_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentPayload;
    use serde::de::IgnoredAny;

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum HelloWithoutJoinIntent {
        Hello {
            window_id: Option<WindowId>,
            clipboard_gating: bool,
            takeover: bool,
            terminal_images: TerminalImageCapabilities,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum HelloWithoutCiRunBar {
        Hello {
            window_id: Option<WindowId>,
            clipboard_gating: bool,
            takeover: bool,
            join_window: bool,
            terminal_images: TerminalImageCapabilities,
        },
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type")]
    enum HelloWithoutPiProvider {
        Hello {
            window_id: Option<WindowId>,
            clipboard_gating: bool,
            takeover: bool,
            join_window: bool,
            terminal_images: TerminalImageCapabilities,
            ci_run_bar: bool,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum WelcomeWithoutBeadsDetail {
        Welcome {
            window_id: WindowId,
            other_windows: Vec<WindowId>,
            clipboard_gating: bool,
            participant_id: Option<u64>,
            terminal_images: TerminalImageCapabilities,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum WelcomeWithoutBeadsWrite {
        Welcome {
            window_id: WindowId,
            other_windows: Vec<WindowId>,
            clipboard_gating: bool,
            participant_id: Option<u64>,
            terminal_images: TerminalImageCapabilities,
            beads_detail: bool,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum HelloWithoutAgentApi {
        Hello {
            window_id: Option<WindowId>,
            clipboard_gating: bool,
            takeover: bool,
            join_window: bool,
            terminal_images: TerminalImageCapabilities,
            ci_run_bar: bool,
            pi_provider: bool,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum WelcomeWithoutAgentApi {
        Welcome {
            window_id: WindowId,
            other_windows: Vec<WindowId>,
            clipboard_gating: bool,
            participant_id: Option<u64>,
            terminal_images: TerminalImageCapabilities,
            beads_detail: bool,
            beads_write: bool,
            beads_flow: bool,
            pi_provider: bool,
        },
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type")]
    enum WelcomeWithoutPiProvider {
        Welcome {
            window_id: WindowId,
            other_windows: Vec<WindowId>,
            clipboard_gating: bool,
            participant_id: Option<u64>,
            terminal_images: TerminalImageCapabilities,
            beads_detail: bool,
            beads_write: bool,
        },
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum WelcomeWithoutBeadsFlow {
        Welcome {
            window_id: WindowId,
            other_windows: Vec<WindowId>,
            clipboard_gating: bool,
            participant_id: Option<u64>,
            terminal_images: TerminalImageCapabilities,
            beads_detail: bool,
            beads_write: bool,
            pi_provider: bool,
        },
    }

    // @lat: [[test#Test Harness#Pi Provider Compatibility#Local capability negotiation]]
    #[test]
    fn old_hello_and_welcome_default_pi_provider_to_false() {
        let hello = HelloWithoutPiProvider::Hello {
            window_id: Some(WindowId::new()),
            clipboard_gating: true,
            takeover: false,
            join_window: false,
            terminal_images: TerminalImageCapabilities::V1,
            ci_run_bar: true,
        };
        let hello_bytes = rmp_serde::to_vec_named(&hello).expect("serialize old Hello");
        let decoded_hello: ClientMessage =
            rmp_serde::from_slice(&hello_bytes).expect("deserialize old Hello");
        assert!(matches!(decoded_hello, ClientMessage::Hello { pi_provider: false, .. }));

        let welcome = WelcomeWithoutPiProvider::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: true,
            beads_write: true,
        };
        let welcome_bytes = rmp_serde::to_vec_named(&welcome).expect("serialize old Welcome");
        let decoded_welcome: ServerMessage =
            rmp_serde::from_slice(&welcome_bytes).expect("deserialize old Welcome");
        assert!(matches!(decoded_welcome, ServerMessage::Welcome { pi_provider: false, .. }));
    }

    #[test]
    fn old_peers_ignore_new_pi_provider_capability_fields() {
        let hello = ClientMessage::Hello {
            window_id: Some(WindowId::new()),
            clipboard_gating: true,
            takeover: false,
            join_window: false,
            terminal_images: TerminalImageCapabilities::V1,
            ci_run_bar: true,
            pi_provider: true,
            agent_api: true,
        };
        let hello_bytes = rmp_serde::to_vec_named(&hello).expect("serialize new Hello");
        let _: HelloWithoutPiProvider =
            rmp_serde::from_slice(&hello_bytes).expect("old client schema decodes new Hello");

        let welcome = ServerMessage::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: true,
            beads_write: true,
            beads_flow: true,
            pi_provider: true,
            agent_api: true,
        };
        let welcome_bytes = rmp_serde::to_vec_named(&welcome).expect("serialize new Welcome");
        let _: WelcomeWithoutPiProvider =
            rmp_serde::from_slice(&welcome_bytes).expect("old server schema decodes new Welcome");
        let decoded_welcome: ServerMessage =
            rmp_serde::from_slice(&welcome_bytes).expect("new server schema decodes Welcome");
        assert!(matches!(decoded_welcome, ServerMessage::Welcome { agent_api: true, .. }));
    }

    // @lat: [[protocol#Protocol#Client Messages#Connection]]
    #[test]
    fn old_hello_and_welcome_default_agent_api_to_false() {
        let hello = HelloWithoutAgentApi::Hello {
            window_id: Some(WindowId::new()),
            clipboard_gating: true,
            takeover: false,
            join_window: false,
            terminal_images: TerminalImageCapabilities::V1,
            ci_run_bar: true,
            pi_provider: true,
        };
        let hello_bytes = rmp_serde::to_vec_named(&hello).expect("serialize old Hello");
        let decoded_hello: ClientMessage =
            rmp_serde::from_slice(&hello_bytes).expect("deserialize old Hello");
        assert!(matches!(decoded_hello, ClientMessage::Hello { agent_api: false, .. }));

        let welcome = WelcomeWithoutAgentApi::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: true,
            beads_write: true,
            beads_flow: true,
            pi_provider: true,
        };
        let welcome_bytes = rmp_serde::to_vec_named(&welcome).expect("serialize old Welcome");
        let decoded_welcome: ServerMessage =
            rmp_serde::from_slice(&welcome_bytes).expect("deserialize old Welcome");
        assert!(matches!(decoded_welcome, ServerMessage::Welcome { agent_api: false, .. }));
    }

    #[test]
    fn new_agent_wire_frames_round_trip() {
        let request = ClientMessage::AgentRequest(AgentRequest::Capabilities {
            request_id: 7,
            agent_label: "test-agent".into(),
            origin_session_id: None,
        });
        let request_bytes = rmp_serde::to_vec_named(&request).expect("serialize agent request");
        let request_decoded: ClientMessage =
            rmp_serde::from_slice(&request_bytes).expect("deserialize agent request");
        assert!(matches!(
            request_decoded,
            ClientMessage::AgentRequest(AgentRequest::Capabilities { request_id: 7, .. })
        ));

        let response = ServerMessage::AgentResponse(AgentResponse {
            request_id: 7,
            result: Ok(AgentPayload::Capabilities {
                capabilities: vec![AgentCapability::ReadMetadata],
            }),
        });
        let response_bytes = rmp_serde::to_vec_named(&response).expect("serialize agent response");
        let response_decoded: ServerMessage =
            rmp_serde::from_slice(&response_bytes).expect("deserialize agent response");
        assert!(matches!(
            response_decoded,
            ServerMessage::AgentResponse(AgentResponse { request_id: 7, .. })
        ));

        for message in [
            ClientMessage::AgentPromptResponse {
                prompt_id: PromptId(8),
                decision: ClipboardDecision::AllowOnce,
            },
            ClientMessage::ActionCompleted {
                correlation_id: 9,
                outcome: AgentActionOutcome::Completed,
                created_session_id: None,
            },
        ] {
            let client_bytes =
                rmp_serde::to_vec_named(&message).expect("serialize agent client frame");
            let _: ClientMessage =
                rmp_serde::from_slice(&client_bytes).expect("deserialize agent client frame");
        }

        for message in [
            ServerMessage::AgentPromptRequest {
                prompt_id: PromptId(10),
                agent_label: "test-agent".into(),
                capability: AgentCapability::ReadMetadata,
                target: "server".into(),
            },
            ServerMessage::AgentActivity { session_id: SessionId::new(), active: true },
            ServerMessage::RunActionCorrelated {
                correlation_id: 11,
                action: AutomationAction::OpenSettings,
            },
        ] {
            let server_bytes =
                rmp_serde::to_vec_named(&message).expect("serialize agent server frame");
            let _: ServerMessage =
                rmp_serde::from_slice(&server_bytes).expect("deserialize agent server frame");
        }
    }

    #[test]
    fn pi_compatibility_helpers_choose_structured_or_legacy_metadata() {
        let (structured, structured_tool) = pi_launch_metadata(true);
        assert!(matches!(
            structured,
            Some(AiLaunchSpec {
                provider: AiProvider::Pi,
                resume_mode: AiResumeMode::New,
                conversation_id: None,
            })
        ));
        assert_eq!(structured_tool, None);

        let (legacy, legacy_tool) = pi_launch_metadata(false);
        assert_eq!(legacy, None);
        assert_eq!(legacy_tool, Some(ShellTool::Pi));

        let session_id = SessionId::new();
        let mut ai_state = AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing);
        ai_state.context = Some(42);
        let mut message = ServerMessage::SessionList {
            sessions: vec![SessionInfo {
                session_id,
                workspace_id: WorkspaceId::new(),
                launch_id: Some("launch-pi".to_owned()),
                shell_name: "bash".to_owned(),
                title: None,
                icon_title: None,
                context: None,
                task_label: Some("ship Pi".to_owned()),
                codex_task_label: None,
                cwd: None,
                git_branch: None,
                ai_state: Some(ai_state),
                ai_provider_hint: Some(AiProvider::Pi),
                shell_tool: None,
                prompt_state: Some(SessionPromptState::default()),
            }],
            workspace_tree: None,
            workspaces: Vec::new(),
        };
        assert!(message.make_pi_provider_compatible(false));
        let ServerMessage::SessionList { sessions, .. } = message else {
            panic!("expected SessionList");
        };
        let session = sessions.first().expect("one session");
        assert_eq!(session.shell_tool, Some(ShellTool::Pi));
        assert!(session.ai_state.is_none());
        assert!(session.ai_provider_hint.is_none());
        assert!(session.task_label.is_none());
        assert!(session.prompt_state.is_none());

        let mut live = ServerMessage::AiStateChanged {
            session_id,
            ai_state: AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing),
        };
        assert!(!live.make_pi_provider_compatible(false));
    }

    #[test]
    fn remote_protocol_advances_for_beads_flow_capability() {
        assert_eq!(REMOTE_PROTOCOL_VERSION, 9);
    }

    /// The bump must keep the refusal legible: a v8 dialer meeting this server
    /// is told both versions and which side to update, not just "incompatible".
    #[test]
    fn version_mismatch_refusal_names_both_versions() {
        let dialer_version = REMOTE_PROTOCOL_VERSION - 1;
        let mismatch = RemoteProtocolMismatch::between(dialer_version, REMOTE_PROTOCOL_VERSION)
            .expect("differing versions mismatch");
        assert_eq!(mismatch.client_version, dialer_version);
        assert_eq!(mismatch.server_version, REMOTE_PROTOCOL_VERSION);

        let reply = ServerMessage::RemoteHandshakeReply {
            accepted: false,
            refusal: Some(RemoteRefusal::IncompatibleVersion),
            server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            server_scribe_version: "0.1.0".into(),
            version_mismatch: Some(mismatch),
        };
        let bytes = rmp_serde::to_vec_named(&reply).expect("serialize mismatch refusal");
        let decoded: ServerMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize mismatch refusal");
        assert!(matches!(
            decoded,
            ServerMessage::RemoteHandshakeReply {
                accepted: false,
                refusal: Some(RemoteRefusal::IncompatibleVersion),
                server_remote_protocol_version,
                version_mismatch: Some(decoded_mismatch),
                ..
            } if server_remote_protocol_version == REMOTE_PROTOCOL_VERSION
                && decoded_mismatch.client_version == dialer_version
                && decoded_mismatch.server_version == REMOTE_PROTOCOL_VERSION
        ));

        assert_eq!(
            RemoteProtocolMismatch::between(REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION),
            None
        );
    }

    #[test]
    fn hello_without_join_intent_defaults_to_false() {
        let bytes = rmp_serde::to_vec_named(&HelloWithoutJoinIntent::Hello {
            window_id: Some(WindowId::new()),
            clipboard_gating: true,
            takeover: false,
            terminal_images: TerminalImageCapabilities::V1,
        })
        .expect("serialize old Hello");

        let decoded: ClientMessage = rmp_serde::from_slice(&bytes).expect("deserialize old Hello");
        assert!(matches!(decoded, ClientMessage::Hello { join_window: false, .. }));
    }

    // @lat: [[protocol#Client Messages#CI Run State#Backward-compatible negotiation]]
    #[test]
    fn hello_without_ci_run_bar_capability_defaults_to_false() {
        let bytes = rmp_serde::to_vec_named(&HelloWithoutCiRunBar::Hello {
            window_id: Some(WindowId::new()),
            clipboard_gating: true,
            takeover: false,
            join_window: false,
            terminal_images: TerminalImageCapabilities::V1,
        })
        .expect("serialize old Hello");

        let decoded: ClientMessage = rmp_serde::from_slice(&bytes).expect("deserialize old Hello");
        assert!(matches!(decoded, ClientMessage::Hello { ci_run_bar: false, .. }));
    }

    // @lat: [[protocol#Server Messages#Connection#Beads detail capability defaults safely]]
    #[test]
    fn welcome_without_beads_detail_capability_defaults_to_false() {
        let bytes = rmp_serde::to_vec_named(&WelcomeWithoutBeadsDetail::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
        })
        .expect("serialize old Welcome");

        let decoded: ServerMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize old Welcome");
        assert!(matches!(decoded, ServerMessage::Welcome { beads_detail: false, .. }));
    }

    // @lat: [[protocol#Server Messages#CI Run State#State message round trip]]
    #[test]
    fn ci_run_state_round_trips_through_msgpack_named() {
        let state = CiRunState {
            repository: "acme/scribe".into(),
            head_sha: "0123456789abcdef".into(),
            branch: "main".into(),
            workflows: vec![CiWorkflowRun {
                run_id: 42,
                name: "quality".into(),
                status: CiWorkflowStatus::Completed,
                conclusion: Some(CiRunConclusion::Failure),
                started_at_epoch_secs: Some(1_723_600_000),
                updated_at_epoch_secs: Some(1_723_600_030),
            }],
            rollup: CiRunStatus::Failure,
            stale: true,
        };
        let message = ServerMessage::CiRunState {
            repo_root: PathBuf::from("/work/scribe"),
            delta: CiRunDelta::Set(state.clone()),
        };

        let bytes = rmp_serde::to_vec_named(&message).expect("serialize CI state");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("deserialize CI state");

        assert!(matches!(
            decoded,
            ServerMessage::CiRunState { repo_root, delta: CiRunDelta::Set(decoded) }
                if repo_root == std::path::Path::new("/work/scribe") && decoded == state
        ));

        let clear_message = ServerMessage::CiRunState {
            repo_root: PathBuf::from("/work/scribe"),
            delta: CiRunDelta::Cleared { head_sha: "head-a".into() },
        };
        let clear_bytes = rmp_serde::to_vec_named(&clear_message).expect("serialize CI clear");
        let decoded_clear: ServerMessage =
            rmp_serde::from_slice(&clear_bytes).expect("deserialize CI clear");

        assert!(matches!(
            decoded_clear,
            ServerMessage::CiRunState {
                repo_root,
                delta: CiRunDelta::Cleared { head_sha }
            } if repo_root == std::path::Path::new("/work/scribe") && head_sha == "head-a"
        ));
    }

    // @lat: [[protocol#Client Messages#CI Run State#Dismiss message round trip]]
    #[test]
    fn dismiss_ci_run_round_trips_through_msgpack_named() {
        let message = ClientMessage::DismissCiRun {
            repo_root: PathBuf::from("/work/scribe"),
            head_sha: "0123456789abcdef".into(),
        };

        let bytes = rmp_serde::to_vec_named(&message).expect("serialize CI dismissal");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize CI dismissal");

        assert!(matches!(
            decoded,
            ClientMessage::DismissCiRun { repo_root, head_sha }
                if repo_root == std::path::Path::new("/work/scribe")
                    && head_sha == "0123456789abcdef"
        ));
    }

    // @lat: [[protocol#Client Messages#CI Run State#Detail-interest message round trip]]
    #[test]
    fn ci_run_details_interest_round_trips_through_msgpack_named() {
        let message = ClientMessage::SetCiRunDetailsInterest {
            repo_root: PathBuf::from("/work/scribe"),
            head_sha: "0123456789abcdef".into(),
            interested: true,
        };

        let bytes = rmp_serde::to_vec_named(&message).expect("serialize CI detail interest");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize CI detail interest");

        assert!(matches!(
            decoded,
            ClientMessage::SetCiRunDetailsInterest { repo_root, head_sha, interested: true }
                if repo_root == std::path::Path::new("/work/scribe")
                    && head_sha == "0123456789abcdef"
        ));
    }

    // @lat: [[protocol#Server Messages#CI Run State#Job-detail message round trip]]
    #[test]
    fn ci_run_details_round_trip_through_msgpack_named() {
        let details = CiRunDetails {
            head_sha: "0123456789abcdef".into(),
            jobs: vec![CiJob {
                job_id: 7,
                workflow_run_id: 42,
                workflow_name: "quality".into(),
                name: "rust-linux".into(),
                status: CiWorkflowStatus::InProgress,
                conclusion: None,
                started_at_epoch_secs: Some(1_723_600_000),
                completed_at_epoch_secs: None,
                steps: vec![CiJobStep {
                    name: "cargo test".into(),
                    status: CiWorkflowStatus::InProgress,
                    conclusion: None,
                }],
            }],
        };
        let message = ServerMessage::CiRunDetails {
            repo_root: PathBuf::from("/work/scribe"),
            details: details.clone(),
        };

        let bytes = rmp_serde::to_vec_named(&message).expect("serialize CI details");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("deserialize CI details");

        assert!(matches!(
            decoded,
            ServerMessage::CiRunDetails { repo_root, details: decoded }
                if repo_root == std::path::Path::new("/work/scribe") && decoded == details
        ));
    }

    // @lat: [[client#Client#Beads Board CLI Data Source]]
    #[test]
    fn beads_board_messages_round_trip_through_msgpack_named() {
        let workspace_id = WorkspaceId::new();
        let request = ClientMessage::RequestBeadsBoard {
            workspace_id,
            protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
        };
        let bytes = rmp_serde::to_vec_named(&request).expect("serialize board request");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize board request");
        assert!(matches!(
            decoded,
            ClientMessage::RequestBeadsBoard {
                workspace_id: decoded_id,
                protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
            } if decoded_id == workspace_id
        ));

        let item = BeadsBoardItem {
            id: "scribe-1bf.2".into(),
            title: "Add board cache".into(),
            priority: 2,
            blocker_ids: vec!["scribe-blocker".into()],
            parent_epic_name: Some("Workspace board".into()),
            parent_epic_id: Some("scribe-1bf".into()),
        };
        let response = ServerMessage::BeadsBoard {
            workspace_id,
            protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
            state: BeadsBoardState::Ready {
                snapshot: BeadsBoardSnapshot {
                    refreshed_at_epoch_ms: 42,
                    blocked: vec![item.clone()],
                    ..BeadsBoardSnapshot::default()
                },
                stale: true,
                refresh_error: Some("refresh timed out".into()),
            },
        };
        let response_bytes = rmp_serde::to_vec_named(&response).expect("serialize board response");
        let decoded_response: ServerMessage =
            rmp_serde::from_slice(&response_bytes).expect("deserialize board response");
        assert!(matches!(
            decoded_response,
            ServerMessage::BeadsBoard {
                workspace_id: decoded_id,
                protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
                state: BeadsBoardState::Ready {
                    snapshot: BeadsBoardSnapshot { blocked, .. },
                    stale: true,
                    refresh_error: Some(_),
                },
            } if decoded_id == workspace_id && blocked == vec![item]
        ));
    }

    fn sample_beads_issue_detail() -> BeadsIssueDetail {
        BeadsIssueDetail {
            id: "scribe-5wh1.2".into(),
            title: "Add issue detail protocol".into(),
            description: "Carry the complete issue.".into(),
            acceptance_criteria: "Named MessagePack round trips.".into(),
            notes: "Read-only slice.".into(),
            design: "Server derives the queue.".into(),
            spec_id: Some("024-beads-card-detail".into()),
            status: "open".into(),
            priority: 1,
            issue_type: "task".into(),
            labels: vec!["protocol".into(), "beads".into()],
            parent_epic_name: Some("Beads card detail".into()),
            assignee: Some("mamba".into()),
            owner: Some("maintainer".into()),
            created_at: "2026-08-14T18:00:00Z".into(),
            updated_at: "2026-08-15T04:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            defer_until: Some("2026-08-16T00:00:00Z".into()),
            due_at: Some("2026-08-20T00:00:00Z".into()),
            estimated_minutes: Some(90),
            external_ref: Some("GH-24".into()),
            blockers: vec![BeadsIssueLink {
                id: "scribe-blocker".into(),
                title: "Select the read contract".into(),
            }],
            dependents: vec![BeadsIssueLink {
                id: "scribe-dependent".into(),
                title: "Render the detail panel".into(),
            }],
            comments: vec![BeadsIssueComment {
                author: "maintainer".into(),
                created_at: "2026-08-15T04:01:00Z".into(),
                body: "Ship the read slice first.".into(),
            }],
            hidden_comment_count: 7,
            queue: BeadsIssueQueue::Blocked,
            queue_basis: BeadsIssueQueueBasis::OpenBlockers,
        }
    }

    // @lat: [[protocol#Client Messages#Beads issue detail#Named MessagePack round trip]]
    #[test]
    fn beads_issue_detail_messages_round_trip_through_msgpack_named() {
        let workspace_id = WorkspaceId::new();
        let request = ClientMessage::RequestBeadsIssueDetail {
            workspace_id,
            issue_id: "scribe-5wh1.2".into(),
        };
        let request_bytes =
            rmp_serde::to_vec_named(&request).expect("serialize issue detail request");
        let decoded_request: ClientMessage =
            rmp_serde::from_slice(&request_bytes).expect("deserialize issue detail request");
        assert!(matches!(
            decoded_request,
            ClientMessage::RequestBeadsIssueDetail { workspace_id: decoded_id, issue_id }
                if decoded_id == workspace_id && issue_id == "scribe-5wh1.2"
        ));

        let detail = sample_beads_issue_detail();
        let response = ServerMessage::BeadsIssueDetail {
            workspace_id,
            issue_id: detail.id.clone(),
            detail: Some(Box::new(detail.clone())),
        };
        let response_bytes =
            rmp_serde::to_vec_named(&response).expect("serialize issue detail response");
        let decoded_response: ServerMessage =
            rmp_serde::from_slice(&response_bytes).expect("deserialize issue detail response");
        assert!(matches!(
            decoded_response,
            ServerMessage::BeadsIssueDetail {
                workspace_id: decoded_id,
                issue_id,
                detail: Some(decoded),
            } if decoded_id == workspace_id
                && issue_id == "scribe-5wh1.2"
                && *decoded == detail
        ));

        let not_found = ServerMessage::BeadsIssueDetail {
            workspace_id,
            issue_id: "scribe-vanished".into(),
            detail: None,
        };
        let not_found_bytes =
            rmp_serde::to_vec_named(&not_found).expect("serialize missing issue detail");
        let decoded_not_found: ServerMessage =
            rmp_serde::from_slice(&not_found_bytes).expect("deserialize missing issue detail");
        assert!(matches!(
            decoded_not_found,
            ServerMessage::BeadsIssueDetail {
                workspace_id: decoded_id,
                issue_id,
                detail: None,
            } if decoded_id == workspace_id && issue_id == "scribe-vanished"
        ));
    }

    // @lat: [[protocol#Client Messages#Beads issue writes#Named MessagePack round trip]]
    #[test]
    fn beads_issue_write_messages_round_trip_every_verb_and_result() {
        let workspace_id = WorkspaceId::new();
        let guard_sets = [
            BeadsIssueWriteGuards {
                if_status: Some("open".into()),
                if_assignee: Some("mamba".into()),
            },
            BeadsIssueWriteGuards::default(),
        ];
        let verbs = vec![
            BeadsIssueWrite::SetTitle { title: "Protocol writes".into() },
            BeadsIssueWrite::SetDescription { description: "Typed client intent.".into() },
            BeadsIssueWrite::SetAcceptance { acceptance: "Every verb round-trips.".into() },
            BeadsIssueWrite::SetNotes { notes: "No server execution yet.".into() },
            BeadsIssueWrite::SetDesign { design: "Server composes argv.".into() },
            BeadsIssueWrite::SetSpecId { spec_id: Some("024-beads-card-detail".into()) },
            BeadsIssueWrite::SetPriority { priority: 1 },
            BeadsIssueWrite::SetType { issue_type: "task".into() },
            BeadsIssueWrite::SetLabels { labels: vec!["protocol".into(), "beads".into()] },
            BeadsIssueWrite::SetStatus { status: "open".into(), clear_defer: true },
            BeadsIssueWrite::Claim,
            BeadsIssueWrite::CloseIssue,
            BeadsIssueWrite::UndoClose,
            BeadsIssueWrite::AddComment { body: "Ship the protocol first.".into() },
        ];

        for expected in verbs {
            for guards in &guard_sets {
                let message = ClientMessage::BeadsIssueWrite {
                    workspace_id,
                    issue_id: "scribe-5wh1.11".into(),
                    verb: expected.clone(),
                    guards: guards.clone(),
                };
                let bytes = rmp_serde::to_vec_named(&message).expect("serialize issue write");
                let decoded: ClientMessage =
                    rmp_serde::from_slice(&bytes).expect("deserialize issue write");
                assert!(matches!(
                    decoded,
                    ClientMessage::BeadsIssueWrite {
                        workspace_id: decoded_id,
                        issue_id,
                        verb,
                        guards: decoded_guards,
                    } if decoded_id == workspace_id
                        && issue_id == "scribe-5wh1.11"
                        && verb == expected
                        && decoded_guards == *guards
                ));
            }
        }

        let results = vec![
            BeadsIssueWriteResult::Applied { generation: 42 },
            BeadsIssueWriteResult::PreconditionFailed,
            BeadsIssueWriteResult::Failed { reason: "bd exited 1".into() },
        ];
        for expected in results {
            let message = ServerMessage::BeadsIssueWriteResult {
                workspace_id,
                issue_id: "scribe-5wh1.11".into(),
                result: expected.clone(),
            };
            let bytes = rmp_serde::to_vec_named(&message).expect("serialize issue write result");
            let decoded: ServerMessage =
                rmp_serde::from_slice(&bytes).expect("deserialize issue write result");
            assert!(matches!(
                decoded,
                ServerMessage::BeadsIssueWriteResult {
                    workspace_id: decoded_id,
                    issue_id,
                    result,
                } if decoded_id == workspace_id
                    && issue_id == "scribe-5wh1.11"
                    && result == expected
            ));
        }
    }

    // @lat: [[protocol#Server Messages#Connection#Beads write capability defaults safely]]
    #[test]
    fn beads_write_capability_is_independent_from_detail() {
        let detail_bytes = rmp_serde::to_vec_named(&WelcomeWithoutBeadsWrite::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: true,
        })
        .expect("serialize detail-only Welcome");
        let decoded_detail: ServerMessage =
            rmp_serde::from_slice(&detail_bytes).expect("deserialize detail-only Welcome");
        assert!(matches!(
            decoded_detail,
            ServerMessage::Welcome { beads_detail: true, beads_write: false, .. }
        ));

        let message = ServerMessage::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: false,
            beads_write: true,
            beads_flow: false,
            pi_provider: true,
            agent_api: false,
        };
        let write_bytes = rmp_serde::to_vec_named(&message).expect("serialize write-only Welcome");
        let decoded_write: ServerMessage =
            rmp_serde::from_slice(&write_bytes).expect("deserialize write-only Welcome");
        assert!(matches!(
            decoded_write,
            ServerMessage::Welcome { beads_detail: false, beads_write: true, .. }
        ));
    }

    // @lat: [[protocol#Server Messages#Connection#Beads flow capability defaults safely]]
    #[test]
    fn beads_flow_capability_is_independent_from_detail_and_write() {
        let legacy_bytes = rmp_serde::to_vec_named(&WelcomeWithoutBeadsFlow::Welcome {
            window_id: WindowId::new(),
            other_windows: Vec::new(),
            clipboard_gating: true,
            participant_id: None,
            terminal_images: TerminalImageCapabilities::V1,
            beads_detail: true,
            beads_write: true,
            pi_provider: true,
        })
        .expect("serialize pre-flow Welcome");
        let decoded_legacy: ServerMessage =
            rmp_serde::from_slice(&legacy_bytes).expect("deserialize pre-flow Welcome");
        assert!(matches!(
            decoded_legacy,
            ServerMessage::Welcome { beads_detail: true, beads_write: true, beads_flow: false, .. }
        ));

        // Every combination is representable: flow is not implied by, and does
        // not imply, either older capability.
        for (detail, write, flow) in
            [(false, false, true), (true, false, true), (false, true, false), (true, true, true)]
        {
            let message = ServerMessage::Welcome {
                window_id: WindowId::new(),
                other_windows: Vec::new(),
                clipboard_gating: true,
                participant_id: None,
                terminal_images: TerminalImageCapabilities::V1,
                beads_detail: detail,
                beads_write: write,
                beads_flow: flow,
                pi_provider: true,
                agent_api: false,
            };
            let bytes = rmp_serde::to_vec_named(&message).expect("serialize Welcome");
            let decoded: ServerMessage =
                rmp_serde::from_slice(&bytes).expect("deserialize Welcome");
            let ServerMessage::Welcome {
                beads_detail: got_detail,
                beads_write: got_write,
                beads_flow: got_flow,
                ..
            } = decoded
            else {
                panic!("expected Welcome");
            };
            assert_eq!((got_detail, got_write, got_flow), (detail, write, flow));
        }
    }

    // @lat: [[protocol#Client Messages#Beads epic graph#Parent epic id defaults safely]]
    #[test]
    fn board_item_without_parent_epic_id_defaults_to_none() {
        #[derive(Serialize)]
        struct BoardItemWithoutParentEpicId {
            id: String,
            title: String,
            priority: u8,
            blocker_ids: Vec<String>,
            parent_epic_name: Option<String>,
        }

        let bytes = rmp_serde::to_vec_named(&BoardItemWithoutParentEpicId {
            id: "scribe-1bf.2".into(),
            title: "Add board cache".into(),
            priority: 2,
            blocker_ids: Vec::new(),
            parent_epic_name: Some("Workspace board".into()),
        })
        .expect("serialize pre-flow board item");

        let decoded: BeadsBoardItem =
            rmp_serde::from_slice(&bytes).expect("deserialize pre-flow board item");
        assert_eq!(decoded.parent_epic_id, None);
        assert_eq!(decoded.parent_epic_name.as_deref(), Some("Workspace board"));
    }

    fn sample_beads_epic_graph() -> BeadsEpicGraph {
        BeadsEpicGraph {
            epic_id: "scribe-lpi2".into(),
            epic_title: "beads-flow-view".into(),
            closed: 1,
            total: 3,
            nodes: vec![
                BeadsGraphNode {
                    id: "scribe-lpi2.1".into(),
                    title: "Add Flow protocol types".into(),
                    priority: 1,
                    status: "closed".into(),
                    queue: BeadsIssueQueue::Done,
                    assignee: Some("mamba".into()),
                    updated_at: "2026-08-18T08:00:00Z".into(),
                },
                BeadsGraphNode {
                    id: "scribe-lpi2.3".into(),
                    title: "Build the Flow layout engine".into(),
                    priority: 1,
                    status: "in_progress".into(),
                    queue: BeadsIssueQueue::InProgress,
                    assignee: None,
                    updated_at: "2026-08-18T09:00:00Z".into(),
                },
                BeadsGraphNode {
                    id: "scribe-lpi2.9".into(),
                    title: "Render the Flow view".into(),
                    priority: 1,
                    status: "open".into(),
                    queue: BeadsIssueQueue::Blocked,
                    assignee: None,
                    updated_at: "2026-08-18T09:30:00Z".into(),
                },
            ],
            edges: vec![
                // A satisfied edge: the blocker is closed and `bd blocked` would
                // not report it, but the graph still shows what was waited on.
                BeadsGraphEdge { from: "scribe-lpi2.1".into(), to: "scribe-lpi2.3".into() },
                BeadsGraphEdge { from: "scribe-lpi2.3".into(), to: "scribe-lpi2.9".into() },
            ],
        }
    }

    // @lat: [[protocol#Client Messages#Beads epic graph#Named MessagePack round trip]]
    #[test]
    fn beads_epic_graph_messages_round_trip_through_msgpack_named() {
        let workspace_id = WorkspaceId::new();
        let request =
            ClientMessage::RequestBeadsEpicGraph { workspace_id, epic_id: "scribe-lpi2".into() };
        let request_bytes = rmp_serde::to_vec_named(&request).expect("serialize graph request");
        let decoded_request: ClientMessage =
            rmp_serde::from_slice(&request_bytes).expect("deserialize graph request");
        assert!(matches!(
            decoded_request,
            ClientMessage::RequestBeadsEpicGraph { workspace_id: decoded_id, epic_id }
                if decoded_id == workspace_id && epic_id == "scribe-lpi2"
        ));

        let graph = sample_beads_epic_graph();
        let outcomes = [
            BeadsEpicGraphOutcome::Graph(Box::new(graph.clone())),
            BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::NoEpic },
            BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::Cycle },
            BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::Disconnected },
            BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::ExternalBlocker },
            BeadsEpicGraphOutcome::NoGraph { reason: BeadsEpicGraphRefusal::TooLarge },
            BeadsEpicGraphOutcome::Unavailable { message: "bd not on PATH".into() },
        ];

        for expected in outcomes {
            let message = ServerMessage::BeadsEpicGraph {
                workspace_id,
                epic_id: "scribe-lpi2".into(),
                outcome: expected.clone(),
            };
            let bytes = rmp_serde::to_vec_named(&message).expect("serialize graph outcome");
            let decoded: ServerMessage =
                rmp_serde::from_slice(&bytes).expect("deserialize graph outcome");
            assert!(matches!(
                decoded,
                ServerMessage::BeadsEpicGraph {
                    workspace_id: decoded_id,
                    epic_id,
                    outcome,
                } if decoded_id == workspace_id
                    && epic_id == "scribe-lpi2"
                    && outcome == expected
            ));
        }

        // The graph arm survives field-for-field, including the satisfied edge
        // and the absent assignee that the live halo depends on.
        let message = ServerMessage::BeadsEpicGraph {
            workspace_id,
            epic_id: graph.epic_id.clone(),
            outcome: BeadsEpicGraphOutcome::Graph(Box::new(graph.clone())),
        };
        let bytes = rmp_serde::to_vec_named(&message).expect("serialize graph");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("deserialize graph");
        let ServerMessage::BeadsEpicGraph {
            outcome: BeadsEpicGraphOutcome::Graph(decoded_graph),
            ..
        } = decoded
        else {
            panic!("expected a graph outcome");
        };
        assert_eq!(*decoded_graph, graph);
    }

    // @lat: [[protocol#Server Messages#Focused Beads issue#Named MessagePack round trip]]
    #[test]
    fn issue_focused_round_trips_set_and_clear() {
        let session_id = SessionId::new();
        for issue_id in [Some(String::from("scribe-lpi2.8")), None] {
            let message = ServerMessage::IssueFocused { session_id, issue_id: issue_id.clone() };
            let bytes = rmp_serde::to_vec_named(&message).expect("serialize focused issue");
            let decoded: ServerMessage =
                rmp_serde::from_slice(&bytes).expect("deserialize focused issue");
            assert!(matches!(
                decoded,
                ServerMessage::IssueFocused { session_id: decoded_id, issue_id: decoded_issue }
                    if decoded_id == session_id && decoded_issue == issue_id
            ));
        }
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    enum ClientMessageWithoutAiLaunch {
        CreateSession {
            workspace_id: WorkspaceId,
            split_direction: Option<LayoutDirection>,
            cwd: Option<PathBuf>,
            size: Option<TerminalSize>,
            command: Option<Vec<String>>,
            env_envelope_id: Option<String>,
        },
    }

    #[derive(Serialize)]
    struct SessionInfoWithoutLaunchIdentity {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        shell_name: String,
        title: Option<String>,
        cwd: Option<PathBuf>,
    }

    // @lat: [[protocol#Server Messages#Launch identity is local-only]]
    #[test]
    fn session_info_launch_identity_is_additive_and_omitted_when_absent() {
        let legacy = SessionInfoWithoutLaunchIdentity {
            session_id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            shell_name: "bash".to_owned(),
            title: None,
            cwd: None,
        };
        let bytes = rmp_serde::to_vec_named(&legacy).expect("serialize legacy SessionInfo");
        let decoded: SessionInfo =
            rmp_serde::from_slice(&bytes).expect("decode legacy SessionInfo");
        assert!(decoded.launch_id.is_none());

        let redacted = rmp_serde::to_vec_named(&decoded).expect("serialize redacted SessionInfo");
        assert!(!redacted.windows(b"launch_id".len()).any(|key| key == b"launch_id"));
    }

    fn sample_release() -> Release {
        Release {
            version: "0.4.2".to_string(),
            name: Some("0.4.2 — Releases page".to_string()),
            published_at: "2026-05-09T10:00:00Z".to_string(),
            body_html: "<h2>Highlights</h2>\n<ul><li>x</li></ul>".to_string(),
            prerelease: false,
            html_url: "https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.2".to_string(),
        }
    }

    fn sample_release_no_name() -> Release {
        Release {
            version: "0.4.1".to_string(),
            name: None,
            published_at: "2026-05-01T10:00:00Z".to_string(),
            body_html: String::new(),
            prerelease: true,
            html_url: "https://github.com/sharaf-nassar/scribe/releases/tag/v0.4.1".to_string(),
        }
    }

    /// Holds the named-msgpack tag of a struct serialized through
    /// `#[serde(tag = "type")]` so wire-name assertions can read the tag
    /// without bringing in `serde_json` or `rmpv` as dev-deps.
    #[derive(Deserialize)]
    struct InternalTagOnly {
        #[serde(rename = "type")]
        tag: String,
    }

    // @lat: [[protocol#Client Messages#Session Lifecycle#Structured AI launch survives MessagePack]]
    #[test]
    fn create_session_ai_launch_round_trips_through_msgpack_named() {
        let workspace_id = WorkspaceId::new();
        let original = ClientMessage::CreateSession {
            workspace_id,
            split_direction: None,
            cwd: Some(PathBuf::from("/tmp/project")),
            size: Some(TerminalSize { cols: 120, rows: 40, cell_width: 8, cell_height: 16 }),
            command: None,
            ai_launch: Some(AiLaunchSpec {
                provider: AiProvider::CodexCode,
                resume_mode: AiResumeMode::Resume,
                conversation_id: Some("conversation-42".to_owned()),
            }),
            shell_tool: None,
            env_envelope_id: Some("launch-42".to_owned()),
        };

        let bytes = rmp_serde::to_vec_named(&original).expect("serialize CreateSession");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize CreateSession");

        match decoded {
            ClientMessage::CreateSession {
                workspace_id: decoded_workspace_id,
                ai_launch: Some(ai_launch),
                command,
                ..
            } => {
                assert_eq!(decoded_workspace_id, workspace_id);
                assert_eq!(ai_launch.provider, AiProvider::CodexCode);
                assert_eq!(ai_launch.resume_mode, AiResumeMode::Resume);
                assert_eq!(ai_launch.conversation_id.as_deref(), Some("conversation-42"));
                assert!(command.is_none());
            }
            other => panic!("unexpected variant after round-trip: {other:?}"),
        }
    }

    // @lat: [[protocol#Client Messages#Session Lifecycle#Missing structured AI launch defaults safely]]
    #[test]
    fn create_session_missing_ai_launch_defaults_to_none() {
        let legacy = ClientMessageWithoutAiLaunch::CreateSession {
            workspace_id: WorkspaceId::new(),
            split_direction: None,
            cwd: None,
            size: None,
            command: None,
            env_envelope_id: Some("legacy-launch".to_owned()),
        };

        let bytes = rmp_serde::to_vec_named(&legacy).expect("serialize legacy CreateSession");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize legacy CreateSession");

        assert!(matches!(
            decoded,
            ClientMessage::CreateSession { ai_launch: None, shell_tool: None, .. }
        ));
    }

    // @lat: [[protocol#Client Messages#Session Lifecycle#Launch-only tool intent survives MessagePack]]
    #[test]
    fn create_session_shell_tool_round_trips_through_msgpack_named() {
        let original = ClientMessage::CreateSession {
            workspace_id: WorkspaceId::new(),
            split_direction: None,
            cwd: Some(PathBuf::from("/tmp/project")),
            size: None,
            command: None,
            ai_launch: None,
            shell_tool: Some(ShellTool::Pi),
            env_envelope_id: Some("launch-43".to_owned()),
        };

        let bytes = rmp_serde::to_vec_named(&original).expect("serialize CreateSession");
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize CreateSession");

        // A tool tab carries no argv and no AI intent: the server owns both, and
        // the tool is never tracked as a provider.
        assert!(matches!(
            decoded,
            ClientMessage::CreateSession {
                command: None,
                ai_launch: None,
                shell_tool: Some(ShellTool::Pi),
                ..
            }
        ));
        assert_eq!(ShellTool::Pi.binary_name(), "pi");
    }

    /// Mirror of [`ReleaseListResultState`] used purely to assert the
    /// externally-tagged discriminator a real serde-default-enum produces
    /// on the wire.
    #[derive(Deserialize)]
    enum ReleaseListResultStateTagShadow {
        Fresh(IgnoredAny),
        Stale(IgnoredAny),
        Failed(IgnoredAny),
    }

    #[test]
    fn release_round_trips_through_msgpack_named() {
        let original = sample_release();
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize Release");
        let decoded: Release = rmp_serde::from_slice(&bytes).expect("deserialize Release");
        assert_eq!(decoded, original);

        // None case: confirms the Option<String> field round-trips when absent.
        let original_no_name = sample_release_no_name();
        let bytes_no_name =
            rmp_serde::to_vec_named(&original_no_name).expect("serialize Release no_name");
        let decoded_no_name: Release =
            rmp_serde::from_slice(&bytes_no_name).expect("deserialize Release no_name");
        assert_eq!(decoded_no_name, original_no_name);
    }

    #[test]
    fn release_list_result_state_fresh_round_trips_through_msgpack_named() {
        let original = ReleaseListResultState::Fresh { releases: vec![sample_release()] };
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize Fresh");
        let decoded: ReleaseListResultState =
            rmp_serde::from_slice(&bytes).expect("deserialize Fresh");
        assert_eq!(decoded, original);

        // Wire-name assertion: ReleaseListResultState has no serde attribute
        // so it serializes externally-tagged as `{ "Fresh": { ... } }`.
        // (Same convention as the existing UpdateCheckResultState in this
        // file — see the report from the agent that landed T008.)
        // Decoding through the shadow enum confirms both the tag name and
        // the externally-tagged shape in one step.
        let shadow: ReleaseListResultStateTagShadow =
            rmp_serde::from_slice(&bytes).expect("decode Fresh through shadow enum");
        assert!(matches!(shadow, ReleaseListResultStateTagShadow::Fresh(_)));
    }

    #[test]
    fn release_list_result_state_stale_round_trips_through_msgpack_named() {
        let original = ReleaseListResultState::Stale {
            releases: vec![sample_release(), sample_release_no_name()],
            reason: "GitHub unreachable".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize Stale");
        let decoded: ReleaseListResultState =
            rmp_serde::from_slice(&bytes).expect("deserialize Stale");
        assert_eq!(decoded, original);

        let shadow: ReleaseListResultStateTagShadow =
            rmp_serde::from_slice(&bytes).expect("decode Stale through shadow enum");
        assert!(matches!(shadow, ReleaseListResultStateTagShadow::Stale(_)));
    }

    #[test]
    fn release_list_result_state_failed_round_trips_through_msgpack_named() {
        let original = ReleaseListResultState::Failed {
            reason: "GitHub rate limit reached, retry after 12 minutes".to_string(),
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize Failed");
        let decoded: ReleaseListResultState =
            rmp_serde::from_slice(&bytes).expect("deserialize Failed");
        assert_eq!(decoded, original);

        let shadow: ReleaseListResultStateTagShadow =
            rmp_serde::from_slice(&bytes).expect("decode Failed through shadow enum");
        assert!(matches!(shadow, ReleaseListResultStateTagShadow::Failed(_)));
    }

    #[test]
    fn client_message_list_releases_round_trips_through_msgpack_named() {
        let original = ClientMessage::ListReleases;
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize ListReleases");

        // Wire-name assertion: ClientMessage uses #[serde(tag = "type")] with
        // no rename_all, so the discriminator is the PascalCase variant name.
        let tagged: InternalTagOnly =
            rmp_serde::from_slice(&bytes).expect("decode ListReleases as tag-only");
        assert_eq!(tagged.tag, "ListReleases");

        // Round-trip back to the actual variant.
        let decoded: ClientMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize ListReleases");
        assert!(matches!(decoded, ClientMessage::ListReleases));
    }

    #[test]
    fn server_message_release_list_round_trips_through_msgpack_named() {
        let original = ServerMessage::ReleaseList {
            state: ReleaseListResultState::Fresh { releases: vec![sample_release()] },
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize ReleaseList");

        // Wire-name assertion: ServerMessage uses #[serde(tag = "type")] with
        // no rename_all, so the discriminator is the PascalCase variant name.
        let tagged: InternalTagOnly =
            rmp_serde::from_slice(&bytes).expect("decode ReleaseList as tag-only");
        assert_eq!(tagged.tag, "ReleaseList");

        // Round-trip and confirm the inner state survives byte-for-byte.
        let decoded: ServerMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize ReleaseList");
        match decoded {
            ServerMessage::ReleaseList { state: ReleaseListResultState::Fresh { releases } } => {
                assert_eq!(releases, vec![sample_release()]);
            }
            other => panic!("unexpected variant after round-trip: {other:?}"),
        }
    }

    #[test]
    fn server_message_clipboard_prompt_request_round_trips_through_msgpack_named() {
        // Spec 010 wire-tag stability check: the new ClipboardPromptRequest
        // variant must serialize with its PascalCase discriminator under the
        // ServerMessage `#[serde(tag = "type")]` shape so older peers can
        // detect-and-skip while newer peers round-trip the payload.
        let original = ServerMessage::ClipboardPromptRequest {
            session_id: SessionId::new(),
            request_id: PromptId(42),
            op: ClipboardOp::Write,
            selection: ClipboardSelection::Clipboard,
            preview: Some("hello world".to_string()),
        };
        let bytes = rmp_serde::to_vec_named(&original).expect("serialize ClipboardPromptRequest");

        let tagged: InternalTagOnly =
            rmp_serde::from_slice(&bytes).expect("decode ClipboardPromptRequest as tag-only");
        assert_eq!(tagged.tag, "ClipboardPromptRequest");

        let decoded: ServerMessage =
            rmp_serde::from_slice(&bytes).expect("deserialize ClipboardPromptRequest");
        match decoded {
            ServerMessage::ClipboardPromptRequest {
                request_id, op, selection, preview, ..
            } => {
                assert_eq!(request_id, PromptId(42));
                assert_eq!(op, ClipboardOp::Write);
                assert_eq!(selection, ClipboardSelection::Clipboard);
                assert_eq!(preview.as_deref(), Some("hello world"));
            }
            other => panic!("unexpected variant after round-trip: {other:?}"),
        }
    }
}
