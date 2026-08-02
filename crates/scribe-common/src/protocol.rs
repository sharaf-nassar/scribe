use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ai_state::{AiProcessState, AiProvider};
use crate::config::SharingMode;
use crate::hook;
use crate::ids::{SessionId, WindowId, WorkspaceId};

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
pub const REMOTE_PROTOCOL_VERSION: u32 = 4;

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

/// Structured AI launch intent carried alongside the legacy command argv.
///
/// The server accepts this value now so a later command-builder change can
/// make it authoritative without another protocol shape change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiLaunchSpec {
    pub provider: AiProvider,
    pub resume_mode: AiResumeMode,
    pub conversation_id: Option<String>,
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
        /// Structured AI launch intent. During the dual-write compatibility
        /// window clients also send the legacy `command` argv; until the
        /// server-owned argv builder lands, `command` remains authoritative.
        #[serde(default)]
        ai_launch: Option<AiLaunchSpec>,
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
    /// The server suppressed an ED 3 (clear scrollback) sequence from an AI
    /// session.  The client should reset `display_offset` to 0 so the
    /// viewport snaps to the live terminal, matching the scroll-to-bottom
    /// side-effect of a real ED 3.
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
    /// [`REMOTE_PROTOCOL_VERSION`] from TXT `protovers`; incompatible peers are
    /// filtered before connecting.
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
    use serde::de::IgnoredAny;

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
            command: Some(vec!["sh".to_owned(), "-lic".to_owned(), "exec codex".to_owned()]),
            ai_launch: Some(AiLaunchSpec {
                provider: AiProvider::CodexCode,
                resume_mode: AiResumeMode::Resume,
                conversation_id: Some("conversation-42".to_owned()),
            }),
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
                assert_eq!(
                    command.as_deref().and_then(|argv| argv.first()).map(String::as_str),
                    Some("sh")
                );
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

        assert!(matches!(decoded, ClientMessage::CreateSession { ai_launch: None, .. }));
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
