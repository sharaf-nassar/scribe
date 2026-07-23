//! IPC client connecting to the scribe-server over a Unix socket.
//!
//! Supports multiple concurrent sessions: each pane can create its own
//! session and route keyboard input independently by session ID.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::UNIX_EPOCH;

use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::app::current_identity;
use scribe_common::config::SharingMode;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    AutomationAction, BridgeError, ClientMessage, ClipboardDecision, ClipboardOp,
    ClipboardSelection, EnvStatusState, LanPeerInfo, LanRefusal, ParticipantInfo, PromptId,
    PromptMarkKind, REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, SearchMatch,
    ServerMessage, ShareEndReason, TerminalSize, UpdateProgressState, WindowInfo,
    WorkspaceNotesCollection, WorkspaceNotesMutation,
};
use scribe_common::socket::{handoff_socket_path, server_socket_path};
// Feature 014 (T015): the connecting client reuses the server's LAN device
// identity + the SPKI-pinning mutual-TLS dialer rather than duplicating the
// security-critical verifier. See the module-level follow-up note on
// `start_lan_ipc_thread` about extracting these into a shared crate.
use scribe_server::lan::identity;
use scribe_server::lan::tls::{DeviceId, DevicePins, LanTls};
use tokio::io::AsyncWriteExt as _;
use winit::event_loop::EventLoopProxy;

/// Commands sent from the winit main thread to the IPC background thread.
#[derive(Debug)]
pub enum ClientCommand {
    /// Raw bytes produced by a key press, routed to a specific session.
    KeyInput { session_id: SessionId, data: Vec<u8>, dismisses_attention: bool },
    /// PTY resize notification for a specific session.
    Resize { session_id: SessionId, size: TerminalSize },
    /// Create a new session in the given workspace.
    ///
    /// When `split_direction` is `Some`, the server records the layout
    /// direction so it can be sent back on reconnect.
    ///
    /// `env_envelope_id` is `Some(launch_id)` only for cold-restart replay
    /// driven by `restore_replay`; the server uses it to apply the
    /// persisted environment envelope for that launch. All other
    /// (non-restore) session creation paths pass `None`.
    CreateSession {
        workspace_id: WorkspaceId,
        split_direction: Option<scribe_common::protocol::LayoutDirection>,
        cwd: Option<std::path::PathBuf>,
        size: Option<TerminalSize>,
        command: Option<Vec<String>>,
        env_envelope_id: Option<String>,
    },
    /// Close a session.
    CloseSession { session_id: SessionId },
    /// Subscribe to output from additional sessions.
    Subscribe { session_ids: Vec<SessionId> },
    /// Request a list of all live sessions on the server.
    ListSessions,
    /// Attach to existing (detached) sessions on the server.
    AttachSessions { session_ids: Vec<SessionId>, dimensions: Vec<TerminalSize> },
    /// Notify server that config file has been updated.
    ConfigReloaded,
    /// Report the current workspace split tree to the server.
    ReportWorkspaceTree { tree: scribe_common::protocol::WorkspaceTreeNode },
    /// Identify this client window to the server (sent as first message).
    ///
    /// `takeover` is `false` for every normal attach; it is set `true` only by a
    /// displaced client reclaiming its window (feature 013, T017). The local
    /// (owning-side) reclaim swaps the connection in place via
    /// [`start_local_takeover_ipc_thread`] (feature 015), while the remote path
    /// re-dials in a fresh process and sets it in [`remote_ipc_main`]; either way
    /// the fresh `Hello` atomically swaps the server's writer back.
    Hello { window_id: Option<WindowId>, takeover: bool },
    /// Close this window and destroy all its sessions on the server.
    CloseWindow { window_id: WindowId },
    /// Request all clients to save state and quit.
    QuitAll,
    /// User confirmed update — download and install.
    TriggerUpdate,
    /// User dismissed update notification.
    DismissUpdate,
    /// Notify server of pane focus change for CSI focus events.
    FocusChanged { gained: Option<SessionId>, lost: Option<SessionId> },
    /// Search the terminal scrollback/screen.
    SearchRequest { session_id: SessionId, query: String, limit: u32 },
    /// Request authoritative workspace note collections.
    WorkspaceNotesGet { workspace_ids: Vec<WorkspaceId> },
    /// Request a server-side workspace notes mutation.
    WorkspaceNotesMutate { mutation: WorkspaceNotesMutation },
    /// Spec 010: user resolved the OSC 52 confirmation dialog. Forwarded
    /// to the server as `ClientMessage::ClipboardPromptResponse`.
    ClipboardPromptResponse { request_id: PromptId, decision: ClipboardDecision },
    /// Spec 010: reply to a `ServerMessage::ClipboardBridgeReadRequest`
    /// carrying the host clipboard payload (or a `BridgeError` on arboard
    /// failure). Forwarded as `ClientMessage::ClipboardBridgeReadReply`.
    ClipboardBridgeReadReply { request_id: PromptId, payload: Result<String, BridgeError> },
    /// Feature 013 (T014): ask THIS machine's own server for its same-account
    /// online tailnet peers, to populate the connect picker's device list.
    /// Local-Unix-socket only; the server refuses it over TCP.
    ListRemotePeers,
    /// Feature 013 (T022): ask THIS machine's own server for its window list so
    /// the owning-machine status bar can surface which windows a remote peer
    /// controls (FR-009b, SC-006) — including remotely-created windows this
    /// process never hosted. Polled only while `remote.enabled`; the reply is a
    /// [`ServerMessage::WindowList`] carrying per-window controller identity.
    ListWindows,
    /// Feature 014 (T014): ask THIS machine's own server for the LAN peers it has
    /// discovered via mDNS on the current network, to populate the connect
    /// picker's "Local network" source. Local-Unix-socket only; the server refuses
    /// it over any remote transport, exactly like [`Self::ListRemotePeers`].
    ListLanPeers,
    /// Feature 014 (T018): the owning user's decision on a pending LAN device
    /// approval, echoing the `request_id` of the originating
    /// [`ServerMessage::LanApprovalRequest`]. `approve = true` writes a
    /// `TrustedDevice` and lets the held connection proceed; `approve = false`
    /// refuses it ([`LanRefusal::Declined`](scribe_common::protocol::LanRefusal::Declined)).
    /// Forwarded to THIS machine's own server as
    /// [`ClientMessage::LanApprovalDecision`]; the server refuses it over any
    /// remote transport (the GUI, never the remote TLS stream, answers the prompt).
    LanApprovalDecision { request_id: u64, approve: bool },
    /// Feature 015 (T020): a viewer takes input control of a shared window in
    /// [`SharingMode::SharedSingleTypist`]. The server applies the owner's
    /// `control_acquisition` policy — instant transfer under `FreeClaim`, or a
    /// routed request under `RequestAndGrant` — so the client sends the same
    /// message either way and learns the result from the next
    /// [`UiEvent::ShareRoster`] (granted) or [`UiEvent::ControlDenied`]. The
    /// server treats [`ClientMessage::ControlClaim`] and
    /// [`ClientMessage::ControlRequest`] identically, so the client only ever
    /// sends `ControlClaim`. Never sent on the local Unix socket unless this is the
    /// owning machine's own local client claiming control.
    ControlClaim { window_id: WindowId },
    /// Feature 015 (T020): the current holder (or owner) answers a pending control
    /// request. `accept = true` transfers control to `participant_id`; `accept =
    /// false` denies it and the requester is notified. Forwarded as
    /// [`ClientMessage::ControlGrant`].
    ControlGrant { window_id: WindowId, participant_id: u64, accept: bool },
}

/// Feature 013 (T009): typed outcome of a remote dial + preamble handshake,
/// handed to the connect flow (T014) as a [`UiEvent::RemoteDialOutcome`] before
/// the link either becomes a normal session or is torn down.
///
/// `Accepted` transitions the connection into the exact same read/write task
/// loop a local Unix-socket client uses. `Refused` carries the server's typed
/// [`RemoteRefusal`] (each variant maps 1:1 to distinct UX-002 copy).
/// `ConnectionFailure` is the deliberately-merged FR-004 outcome — a refused or
/// timed-out TCP connect, or a link that closed before the reply — i.e. the
/// peer is offline, not running Scribe, or has remote access disabled, made
/// indistinguishable on a cold connect because FR-001 leaves nothing listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteConnectOutcome {
    /// Preamble accepted; the connection now behaves exactly like a local one.
    Accepted,
    /// The server answered with a typed refusal.
    Refused(RemoteRefusal),
    /// Connect refused/timed out, or the link closed before a reply arrived.
    ConnectionFailure,
}

/// Feature 014 (T015): typed outcome of a LAN dial — the mutual-TLS handshake,
/// the `LanHello` preamble, and the owning side's device-approval gate — handed
/// to the connect flow as a [`UiEvent::LanDialOutcome`] before the link either
/// becomes a normal session or is torn down. The LAN analogue of
/// [`RemoteConnectOutcome`] (tailnet), differing only in that a refusal carries a
/// [`LanRefusal`] (the LAN taxonomy: `Declined` / `NotTrustedNetwork` /
/// `Disabled` / `IncompatibleVersion` / `Busy`) rather than a [`RemoteRefusal`].
///
/// The interim "held pending device approval" state is NOT an outcome — it is
/// surfaced separately as [`UiEvent::LanAwaitingApproval`] the moment the peer
/// sends [`ServerMessage::LanApprovalPending`], because the wait for the owning
/// user's decision (up to the peer's approval timeout) precedes this terminal
/// outcome (FR-014, contracts/lan-protocol.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanConnectOutcome {
    /// Approved (or an already-trusted device): the peer sent
    /// [`ServerMessage::LanApprovalResult`] with `approved = true`; the
    /// connection now behaves exactly like any other remote one.
    Accepted,
    /// The peer refused with a typed [`LanRefusal`] (each variant maps 1:1 to
    /// distinct UX-002 copy).
    Refused(LanRefusal),
    /// The TCP connect, the TLS handshake, or the framed exchange failed before a
    /// terminal `LanApprovalResult` arrived — the deliberately-merged connection
    /// failure (peer offline / asleep / not on this network / LAN disabled),
    /// indistinguishable on a cold dial because a dormant peer leaves nothing
    /// listening (contracts/settings-and-config.md).
    ConnectionFailure,
}

/// Events forwarded from the IPC background thread to the winit event loop.
#[derive(Debug)]
pub enum UiEvent {
    /// Raw PTY output bytes for a specific session.
    PtyOutput { session_id: SessionId, data: Vec<u8> },
    /// Full screen snapshot for restoring visible content (used by tooling
    /// like `scribe-cli` / `scribe-test` via `RequestSnapshot`). Reattach uses
    /// `SessionReplay` instead.
    ScreenSnapshot { session_id: SessionId, snapshot: scribe_common::screen::ScreenSnapshot },
    /// Compressed ANSI replay for a reattached session. Feed the decompressed
    /// bytes into the pane's VTE processor to rebuild its `Term` durably.
    SessionReplay { session_id: SessionId, replay: scribe_common::screen_replay::SessionReplay },
    /// The server has acknowledged session creation.
    SessionCreated { session_id: SessionId, shell_name: String },
    /// A session has exited.
    SessionExited { session_id: SessionId },
    /// The AI state for a session has changed.
    AiStateChanged { session_id: SessionId, ai_state: AiProcessState },
    /// The AI state for a session was explicitly cleared.
    AiStateCleared { session_id: SessionId },
    /// The terminal emitted BEL for a session.
    Bell { session_id: SessionId },
    /// The working directory for a session has changed.
    CwdChanged { session_id: SessionId, cwd: PathBuf },
    /// Per-session env-capture runtime state changed (feature 006).
    /// Sent on transitions only; the client mirrors `state` onto the
    /// matching pane's `env_status` field so the status bar can render
    /// the warning glyph when `Degraded`.
    EnvStatus { session_id: SessionId, state: EnvStatusState },
    /// The shell/session context for a session has changed.
    SessionContextChanged {
        session_id: SessionId,
        context: scribe_common::protocol::SessionContext,
    },
    /// The terminal title for a session has changed.
    TitleChanged { session_id: SessionId, title: String },
    /// The active provider task label for a session has changed.
    TaskLabelChanged { session_id: SessionId, provider: AiProvider, task_label: String },
    /// The active provider task label for a session was cleared.
    TaskLabelCleared { session_id: SessionId, provider: AiProvider },
    /// A user prompt was submitted in a supported AI coding session.
    PromptReceived { session_id: SessionId, text: String },
    /// Git branch for a session's CWD (None if not in a git repo).
    GitBranch { session_id: SessionId, branch: Option<String> },
    /// Full workspace state sent from the server.
    WorkspaceInfo {
        workspace_id: WorkspaceId,
        name: Option<String>,
        accent_color: String,
        split_direction: Option<scribe_common::protocol::LayoutDirection>,
        project_root: Option<std::path::PathBuf>,
    },
    /// List of all live sessions, received in response to `ListSessions`.
    /// Carries all per-workspace metadata as a batched `workspaces` vec so the
    /// client can populate names / accent colors before reattach completes.
    SessionList {
        sessions: Vec<scribe_common::protocol::SessionInfo>,
        workspace_tree: Option<scribe_common::protocol::WorkspaceTreeNode>,
        workspaces: Vec<scribe_common::protocol::WorkspaceListEntry>,
    },
    /// A workspace has been auto-named.
    WorkspaceNamed {
        workspace_id: WorkspaceId,
        name: String,
        project_root: Option<std::path::PathBuf>,
    },
    /// Server configuration has been reloaded.
    ConfigChanged,
    /// The connection to the server was lost.
    ServerDisconnected,
    /// Animation timer tick -- sent by the animation thread to drive redraws.
    AnimationTick,
    /// Server confirmed our window identity and listed other windows to spawn.
    /// Feature 015: `participant_id` is this connection's own id in the window's
    /// share (for exact self-identification in a `ShareRoster`), or `None` from an
    /// older server / a lost-control landing.
    Welcome { window_id: WindowId, other_windows: Vec<WindowId>, participant_id: Option<u64> },
    /// Server confirmed that this window was permanently removed.
    WindowClosed { window_id: WindowId },
    /// Server requested us to save state and quit (`QuitAll` was acknowledged).
    QuitRequested,
    /// Server requested that this client execute an automation action.
    RunAction { action: AutomationAction },
    /// Server found an available update.
    UpdateAvailable { version: String },
    /// Update progress changed.
    UpdateProgress { state: UpdateProgressState },
    /// A shell prompt-mark event from OSC 133. `exit_code` is forwarded from
    /// the wire `ServerMessage::PromptMark` and is only meaningful on a
    /// `CommandEnd` (D) mark; `None` means the shell reported no status.
    PromptMark {
        session_id: SessionId,
        kind: PromptMarkKind,
        click_events: bool,
        exit_code: Option<i32>,
    },
    /// Search results for the current query.
    SearchResults { session_id: SessionId, query: String, matches: Vec<SearchMatch> },
    /// The server trimmed duplicate AI redraw scrollback for this session.
    TrimScrollback { session_id: SessionId, history_rows: u32 },
    /// The server suppressed an ED 3 sequence — snap the viewport to bottom.
    ScrollBottom { session_id: SessionId },
    /// Server-authoritative notes snapshot for requested workspaces.
    WorkspaceNotesSnapshot { collections: Vec<WorkspaceNotesCollection> },
    /// Server-authoritative notes collection after a persisted mutation.
    WorkspaceNotesChanged { collection: WorkspaceNotesCollection },
    /// Generic server error.
    ServerError { message: String },
    /// Server confirmed our window identity and recorded the
    /// `clipboard_gating` capability bit. Spec 010 C7; carries the
    /// server's flag so the client can defensively no-op on the new
    /// clipboard variants when `false`.
    ClipboardGatingNegotiated { server_supports: bool },
    /// Spec 010: server asks the user to confirm an OSC 52 read or write.
    /// The client renders a clipboard dialog and replies with
    /// `ClipboardPromptResponse`.
    ClipboardPromptRequest {
        session_id: SessionId,
        request_id: PromptId,
        op: ClipboardOp,
        selection: ClipboardSelection,
        preview: Option<String>,
    },
    /// Spec 010: server forwarded an allowed OSC 52 write payload to be
    /// written to the host clipboard via the bridge.
    ClipboardBridgeWrite {
        /// Wire-side anchor for the originating pane. Wave 2's bridge call
        /// is window-scoped so the field is logged for diagnostic context;
        /// Wave 5's primary-selection / focus-gate work routes on it.
        session_id: SessionId,
        selection: ClipboardSelection,
        payload: String,
    },
    /// Spec 010: server requests the host clipboard for an allowed OSC 52
    /// read. The client replies with `ClipboardBridgeReadReply`.
    ClipboardBridgeReadRequest {
        /// Wire-side anchor for the originating pane. Logged for diagnostic
        /// context until Wave 5 introduces per-pane routing.
        session_id: SessionId,
        request_id: PromptId,
        selection: ClipboardSelection,
    },
    /// Feature 013 (T009): result of a remote dial + preamble handshake,
    /// delivered exactly once before any session traffic. The connect flow
    /// (T014) drives the picker's success / typed-failure copy from `outcome`;
    /// until then the event loop's catch-all ignores it.
    RemoteDialOutcome { outcome: RemoteConnectOutcome },
    /// Feature 013 (T009): the peer sent a `RemoteDisconnect` sever notice
    /// (v1 reason: remote access disabled) just before closing the link. Lets
    /// the UI state the disable as fact rather than inferring it from the drop.
    RemoteSevered { reason: RemoteRefusal },
    /// Feature 013 (T014): reply to a `ListRemotePeers` request — this machine's
    /// same-account tailnet peers for the connect picker's device list.
    RemotePeerList { peers: Vec<RemotePeerInfo> },
    /// Feature 013 (T014): the window list probed from a chosen peer over the
    /// remote link, carried back with the dial target so the picker can match
    /// it to the peer it is showing. Emitted by [`start_remote_list_windows_thread`].
    RemoteWindowList { host: String, port: u16, windows: Vec<WindowInfo> },
    /// Feature 013 (T022): reply to a [`ClientCommand::ListWindows`] poll on THIS
    /// machine's own server. Each entry's `controller` names the remote peer
    /// holding the window (or `None` when locally controlled / unconnected),
    /// feeding the owning-machine remote status surfaces (FR-009b, SC-006).
    LocalWindowList { windows: Vec<WindowInfo> },
    /// Feature 013 (T017): another controller claimed this window. The client
    /// freezes its last frame, suppresses input, and renders the displaced
    /// banner naming the new controller, offering one-action reclaim. Drives
    /// both a local client displaced by a remote peer and a remote client
    /// displaced by a reclaim (contracts/remote-protocol.md displaced-client
    /// obligations).
    WindowTakenOver { device_name: String, login_name: String },
    /// Feature 013 (T030): the remote link for this controlling-side window
    /// dropped and the auto-reconnect loop is retrying with capped exponential
    /// backoff. `attempt` is 1-based and drives the cancelable "Reconnecting to
    /// <peer>… (attempt n)" overlay (FR-011). Ignored once the overlay has
    /// settled terminally (cancel / disabled / gave up).
    RemoteReconnecting { attempt: u32 },
    /// Feature 013 (T030): the auto-reconnect loop re-established the link and
    /// re-sent `Hello { takeover: false }`. The UI clears the reconnecting
    /// overlay and re-requests the session list so a fresh replay rebuilds every
    /// pane (full convergence, FR-011). If the window was taken over during the
    /// outage, the immediate `WindowTakenOver` that follows lands the client in
    /// the lost-control state instead — never a silent seizure.
    RemoteReconnected,
    /// Feature 013 (T030): the auto-reconnect loop exhausted its capped backoff
    /// without reaching the peer. The UI settles into the combined
    /// connection-failure state (offline / not running / disabled) with a
    /// one-action reconnect (contracts Disable semantics, FR-004). The LAN
    /// transport (feature 014) reuses this same transport-agnostic settle.
    RemoteReconnectFailed,
    /// Feature 014 (T015): the LAN dial reached the owning peer over mutual TLS
    /// but the connection is held pending the owning user's device approval — the
    /// peer sent [`ServerMessage::LanApprovalPending`]. Drives the connecting
    /// side's cancelable "Waiting for approval on <peer>…" overlay (FR-014,
    /// US2.5); the terminal [`UiEvent::LanDialOutcome`] follows once the owning
    /// user decides or the hold times out. The overlay + cancel wiring lands with
    /// the picker/pending work (T019); until then the event loop's catch-all
    /// ignores it.
    LanAwaitingApproval,
    /// Feature 014 (T015): terminal outcome of a LAN dial + TLS + approval gate,
    /// delivered exactly once. The LAN analogue of [`UiEvent::RemoteDialOutcome`];
    /// the connect flow (T014) drives the picker's success / typed-failure copy
    /// from `outcome`. Until that lands the event loop's catch-all ignores it.
    LanDialOutcome { outcome: LanConnectOutcome },
    /// Feature 014 (T014): reply to a [`ClientCommand::ListLanPeers`] request —
    /// this machine's mDNS-discovered LAN peers for the connect picker's "Local
    /// network" source, merged with the tailnet [`Self::RemotePeerList`] by
    /// machine name (T024). Emitted by the local-server dispatch below.
    LanPeerList { peers: Vec<LanPeerInfo> },
    /// Feature 014 (T018): an unknown LAN device has completed the mutual-TLS
    /// handshake on THIS (owning) machine and is held pending the user's
    /// approval, carried from [`ServerMessage::LanApprovalRequest`] over the local
    /// socket. Drives the owning-side approval prompt (`lan_approval`), which
    /// replies with a [`ClientCommand::LanApprovalDecision`] echoing `request_id`.
    /// No window or session data flows until the user approves (SEC-001/002).
    LanApprovalRequest {
        request_id: u64,
        device_name: String,
        fingerprint_words: String,
        network_label: String,
        name_collision: bool,
    },
    /// Feature 015 (T015/T024): full-state roster of a shared window, broadcast on
    /// every join, leave, control transfer, ejection, and mode change. Drives the
    /// live-viewer input suppression, the claim/request affordances, and the
    /// presence badge. Received by every participant (including the owning
    /// machine's own local client). Never a delta — always the complete roster.
    ShareRoster {
        window_id: WindowId,
        participants: Vec<ParticipantInfo>,
        mode: SharingMode,
        holder: Option<u64>,
    },
    /// Feature 015 (T020): a viewer requested input control under
    /// `RequestAndGrant`; delivered to this client because it is the current
    /// holder (or the owner when unheld). Drives the grant/deny prompt, answered
    /// with a [`ClientCommand::ControlGrant`].
    ControlRequested { window_id: WindowId, from: ParticipantInfo },
    /// Feature 015 (T020): this client's control request was denied (or cancelled
    /// by a holder / mode change). Drives a transient requester notice.
    ControlDenied { window_id: WindowId },
    /// Feature 015 (T020): the shared session ended for this remote participant —
    /// the owner closed the window/session or flipped to `SingleController`. The
    /// mode-neutral roster/notice signal beside the legacy `WindowTakenOver`
    /// displaced UI a `SingleController` flip also sends.
    ShareEnded { window_id: WindowId, reason: ShareEndReason },
}

/// Cancel switch for a remote window's auto-reconnect loop (feature 013, T030).
///
/// Shared between the winit UI thread and the IPC background thread. The UI sets
/// it when the user cancels the "Reconnecting…" overlay, or when the peer
/// delivers an authoritative `RemoteDisconnect` sever notice — either way the
/// loop must stop retrying against a listener that is gone or unwanted rather
/// than spinning forever (contracts Disable semantics, FR-011). The loop polls
/// it between attempts and during the cancelable backoff. Only a remote-dialed
/// process ever has one; the local Unix-socket path returns `None`.
#[derive(Clone, Default)]
pub struct RemoteReconnectCancel(Arc<AtomicBool>);

impl RemoteReconnectCancel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal the auto-reconnect loop to stop. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What [`start_ipc_thread`] hands back: the command sender every UI path uses,
/// plus — for a remote-dialed process only — the cancel switch for its
/// auto-reconnect loop (feature 013, T030).
pub struct IpcHandle {
    /// Sends [`ClientCommand`]s to the server (local Unix socket or remote TCP).
    pub commands: mpsc::Sender<ClientCommand>,
    /// Cancel switch for the remote auto-reconnect loop; `None` on the local path.
    pub reconnect_cancel: Option<RemoteReconnectCancel>,
}

/// Start the IPC client on a background thread.
///
/// Spawns a `std::thread` that owns a single-threaded Tokio runtime.
/// The runtime connects to the server and bridges server messages to
/// the winit event loop via `proxy`, while routing keyboard / resize /
/// session commands received on the returned sender to the server.
///
/// Returns an [`IpcHandle`] carrying the [`mpsc::Sender<ClientCommand>`] the
/// main thread uses to forward keyboard input, resize events, and session
/// commands, plus the remote auto-reconnect cancel switch when this process
/// dialed a peer (feature 013, T030).
pub fn start_ipc_thread(proxy: EventLoopProxy<UiEvent>, window_id: Option<WindowId>) -> IpcHandle {
    // Feature 013 (T009) plumbing hook: when `SCRIBE_REMOTE_DIAL` names a
    // tailnet peer, dial it over TCP instead of the local Unix socket, sharing
    // the exact read/write task loop below. The connect picker (T014) is the
    // real entry point; this env hook exists so the remote transport can be
    // exercised end-to-end before the UI lands. Unset (the default) keeps the
    // local path byte-for-byte unchanged.
    if let Some((host, port)) = remote_dial_target_from_env() {
        // The connect picker (T014) spawns a fresh client process per remote
        // control window, passing the claim target and takeover flag through the
        // environment: `SCRIBE_REMOTE_WINDOW` names an existing window to claim
        // (absent ⇒ a fresh window, T018), and `SCRIBE_REMOTE_TAKEOVER` marks the
        // explicit-attach path. A process launched by the raw plumbing hook (no
        // picker) keeps its own `--window-id` and never takes over.
        let remote_window = remote_dial_window_from_env().or(window_id);
        let takeover = remote_dial_takeover_from_env();
        tracing::info!(
            %host,
            port,
            ?remote_window,
            takeover,
            "SCRIBE_REMOTE_DIAL set; dialing remote scribe-server"
        );
        let reconnect_cancel = RemoteReconnectCancel::new();
        let dial = RemoteDial { host, port, window_id: remote_window, takeover };
        let commands = start_remote_ipc_thread(proxy, dial, reconnect_cancel.clone());
        return IpcHandle { commands, reconnect_cancel: Some(reconnect_cancel) };
    }

    // Feature 014 (T015) plumbing hook: when `SCRIBE_LAN_DIAL` names a LAN peer
    // (the resolved subnet address + LAN port a `ListLanPeers` entry carries),
    // dial it over mutual TLS instead of the local Unix socket. Mirrors the
    // `SCRIBE_REMOTE_DIAL` hook above so the LAN transport is exercisable before
    // the "Local network" picker source (T014) lands; the reclaim/takeover claim
    // reuses the transport-agnostic `SCRIBE_REMOTE_WINDOW` / `SCRIBE_REMOTE_TAKEOVER`
    // markers. Unset (the default) keeps the local path byte-for-byte unchanged.
    if let Some((host, port)) = lan_dial_target_from_env() {
        let lan_window = remote_dial_window_from_env().or(window_id);
        let takeover = remote_dial_takeover_from_env();
        tracing::info!(
            %host,
            port,
            ?lan_window,
            takeover,
            "SCRIBE_LAN_DIAL set; dialing LAN scribe-server over mutual TLS"
        );
        let reconnect_cancel = RemoteReconnectCancel::new();
        let dial = LanDial { host, port, window_id: lan_window, takeover };
        let commands = start_lan_ipc_thread(proxy, dial, reconnect_cancel.clone());
        return IpcHandle { commands, reconnect_cancel: Some(reconnect_cancel) };
    }

    // Local Unix-socket path. The first `Hello`'s `takeover` reads the
    // `--reclaim` CLI flag (feature 013, T017): true only when this client was
    // launched to reclaim a window it was displaced from; every ordinary launch
    // reads it as false and keeps today's non-displacing behavior.
    start_local_ipc_thread(proxy, window_id, local_takeover_requested())
}

/// Start the LOCAL (Unix-socket) IPC client on a background thread, shared by the
/// ordinary local path of [`start_ipc_thread`] and the in-place reclaim entry
/// point [`start_local_takeover_ipc_thread`].
///
/// Creates the command channel, pre-queues `Hello { window_id, takeover }` as the
/// first frame on the wire, and spawns the single-shot [`ipc_main`] on a
/// dedicated `std::thread` owning a single-threaded Tokio runtime. `takeover` is
/// the ordinary path's `--reclaim` flag, or forced `true` for an in-place reclaim.
fn start_local_ipc_thread(
    proxy: EventLoopProxy<UiEvent>,
    window_id: Option<WindowId>,
    takeover: bool,
) -> IpcHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    // Wrap cmd_rx in Arc<Mutex<_>> so it can be moved into spawn_blocking
    // closures which require 'static bounds.
    let cmd_rx = Arc::new(Mutex::new(cmd_rx));

    // Send Hello as the first command so it's the first message on the wire.
    if cmd_tx.send(ClientCommand::Hello { window_id, takeover }).is_err() {
        tracing::warn!("IPC channel closed before Hello could be sent");
    }

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(error = %error, "failed to create tokio runtime");
                send_event(&proxy, UiEvent::ServerDisconnected);
                return;
            }
        };
        rt.block_on(ipc_main(proxy, cmd_rx));
    });

    IpcHandle { commands: cmd_tx, reconnect_cancel: None }
}

/// Start a fresh LOCAL (Unix-socket) IPC thread whose first `Hello` carries
/// `takeover = true` regardless of the `--reclaim` CLI flag — the in-place
/// reclaim entry point (feature 015).
///
/// A window displaced by a remote controller drops its old command sender and
/// calls this to swap in a fresh takeover connection WITHOUT tearing down its
/// window or event loop, so there is no visible close+reopen (contrast the old
/// spawn-a-fresh-process reclaim). Only the local `Hello`'s `takeover` differs
/// from [`start_ipc_thread`]'s local path; the remote / LAN dial paths are never
/// reached here.
pub fn start_local_takeover_ipc_thread(
    proxy: EventLoopProxy<UiEvent>,
    window_id: Option<WindowId>,
) -> IpcHandle {
    start_local_ipc_thread(proxy, window_id, true)
}

/// Bundled parameters for a remote dial: the tailnet address to reach and the
/// window claim to make once the preamble is accepted. Grouped so
/// [`start_remote_ipc_thread`] / [`remote_ipc_main`] stay within the
/// argument-count budget and the dial target travels as one unit.
pub struct RemoteDial {
    host: String,
    port: u16,
    /// `None` creates a fresh window on the peer; `Some` claims an existing one.
    window_id: Option<WindowId>,
    /// Set only for explicit picker attach / lost-control reclaim, never on the
    /// auto-reconnect path (FR-011).
    takeover: bool,
}

/// Start the IPC client against a REMOTE scribe-server over TCP, on a
/// background thread (feature 013, T009).
///
/// Mirrors [`start_ipc_thread`] — same dedicated `std::thread` owning a
/// single-threaded Tokio runtime, and the same read/write task loop once the
/// link is established — but dials `host:port` on the peer's tailnet address
/// and runs the remote preamble first. It is strictly connect-only: unlike the
/// local path it never starts, refreshes, or upgrades the peer's server (there
/// is no [`connect_or_start_server`] equivalent). The handshake result and any
/// later sever notice surface to the UI as [`UiEvent::RemoteDialOutcome`] /
/// [`UiEvent::RemoteSevered`]; once accepted, every other event is identical to
/// a local session. `window_id: None` creates a fresh window on the peer;
/// `takeover` is set only for explicit picker attach / lost-control reclaim.
pub fn start_remote_ipc_thread(
    proxy: EventLoopProxy<UiEvent>,
    dial: RemoteDial,
    cancel: RemoteReconnectCancel,
) -> mpsc::Sender<ClientCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    // Unlike the local path, `Hello` is NOT pre-queued here: the remote preamble
    // (`RemoteHandshake`) must be the first frame, and `Hello` is sent only
    // after the server accepts. `remote_ipc_main` owns that ordering. The raw
    // receiver is handed straight to `remote_ipc_main`, which owns it for the
    // whole (multi-attempt) lifetime via a single command bridge (T030) — no
    // `Arc<Mutex>` sharing, so a dropped link never strands a blocking `recv`.

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(error = %error, "failed to create tokio runtime for remote IPC");
                send_event(
                    &proxy,
                    UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
                );
                return;
            }
        };
        rt.block_on(remote_ipc_main(proxy, cmd_rx, dial, cancel));
    });

    cmd_tx
}

/// Parse the optional `SCRIBE_REMOTE_DIAL` plumbing hook (`host` or `host:port`,
/// a `MagicDNS` name or IP). Returns `None` when unset or empty so the default
/// local Unix-socket path runs. Feature 013 (T009); superseded by the T014
/// picker.
fn remote_dial_target_from_env() -> Option<(String, u16)> {
    let raw = std::env::var("SCRIBE_REMOTE_DIAL").ok()?;
    let default_port = scribe_common::config::RemoteConfig::default().port;
    parse_dial_target(&raw, default_port)
}

/// Parse a `host` / `host:port` dial-target string shared by the plumbing hooks
/// (`SCRIBE_REMOTE_DIAL`, feature 013; `SCRIBE_LAN_DIAL`, feature 014). Returns
/// `None` when empty so the caller keeps the default local Unix-socket path.
/// Splits on the FINAL colon only when the host carries no colon of its own, so a
/// bare IPv6 literal falls through to `default_port` and is dialed verbatim (a
/// `(host, port)` tuple resolves an unbracketed literal fine).
fn parse_dial_target(raw: &str, default_port: u16) -> Option<(String, u16)> {
    let target = raw.trim();
    if target.is_empty() {
        return None;
    }
    let (host, port) = match target.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(':') => {
            (host, port.parse::<u16>().unwrap_or(default_port))
        }
        _ => (target, default_port),
    };
    Some((host.to_owned(), port))
}

/// Parse the optional `SCRIBE_LAN_DIAL` plumbing hook (`host` or `host:port`; the
/// resolved LAN subnet address a `ListLanPeers` entry carries). Returns `None`
/// when unset or empty so the default local Unix-socket path runs. The default
/// port is the LAN listener port (46062), distinct from the tailnet 46061.
/// Feature 014 (T015); superseded by the T014 "Local network" picker source.
fn lan_dial_target_from_env() -> Option<(String, u16)> {
    let raw = std::env::var("SCRIBE_LAN_DIAL").ok()?;
    let default_port = scribe_common::config::LanRemoteConfig::default().port;
    parse_dial_target(&raw, default_port)
}

/// Parse the optional `SCRIBE_REMOTE_WINDOW` claim target set by the connect
/// picker when it spawns a remote-control client process (feature 013, T014).
/// `None` (unset, empty, or unparsable) creates a fresh window on the peer
/// (T018); `Some` claims that existing window.
fn remote_dial_window_from_env() -> Option<WindowId> {
    let raw = std::env::var("SCRIBE_REMOTE_WINDOW").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<WindowId>() {
        Ok(window_id) => Some(window_id),
        Err(error) => {
            tracing::warn!(%error, value = %trimmed, "invalid SCRIBE_REMOTE_WINDOW; ignoring");
            None
        }
    }
}

/// Whether `SCRIBE_REMOTE_TAKEOVER` marks this as the explicit-attach path that
/// may displace a connected controller (feature 013, T014). Only ever set by the
/// connect picker's attach action, never on the auto-reconnect path (FR-011).
fn remote_dial_takeover_from_env() -> bool {
    env_flag_set("SCRIBE_REMOTE_TAKEOVER")
}

/// Whether this LOCAL (Unix-socket) client process was launched with the
/// `--reclaim` CLI flag (feature 013, T017), forcing its first `Hello` to carry
/// `takeover = true` so the local server swaps the writer back. The owning-side
/// banner's reclaim now swaps the connection IN PLACE via
/// [`start_local_takeover_ipc_thread`] (feature 015) rather than spawning a
/// `--reclaim` process, so this flag is vestigial for in-tree launches but still
/// honored for any manual invocation. A CLI flag rather than an env var keeps the
/// marker from leaking into unrelated child processes this client later spawns
/// (e.g. a new window). The remote side reaches the same claim via
/// `SCRIBE_REMOTE_TAKEOVER`.
fn local_takeover_requested() -> bool {
    std::env::args().any(|arg| arg == "--reclaim")
}

/// Parse a boolean env flag with the `1` / `true` spelling shared by the
/// feature-013 spawn markers.
fn env_flag_set(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// The tailnet dial target of a remote-control client process, if this process
/// was launched pointed at a REMOTE scribe-server (feature 013). Reused by the
/// displaced-client reclaim (T017) to re-dial the same peer with a takeover
/// claim; `None` on an ordinary local client.
#[must_use]
pub fn remote_dial_target() -> Option<(String, u16)> {
    remote_dial_target_from_env()
}

/// The LAN dial target of a remote-control client process, if this process was
/// launched pointed at a LAN scribe-server over mutual TLS (feature 014, T015;
/// `SCRIBE_LAN_DIAL`). The LAN analogue of [`remote_dial_target`], reused by the
/// controlling-window reclaim / one-action reconnect (T016) so a LAN-attached
/// window re-dials over the LAN transport, and by the status bar's transport
/// indicator (T025). `None` on a tailnet-dialed or ordinary local client.
#[must_use]
pub fn lan_dial_target() -> Option<(String, u16)> {
    lan_dial_target_from_env()
}

/// Send a `UiEvent` via the event loop proxy, logging if the event loop is gone.
fn send_event(proxy: &EventLoopProxy<UiEvent>, event: UiEvent) {
    if proxy.send_event(event).is_err() {
        tracing::warn!("winit event loop closed; dropping event");
    }
}

/// Per-transport behavior for the shared read loop [`run_read_task`]. The local
/// Unix-socket path reports link loss as [`UiEvent::ServerDisconnected`] and
/// captures nothing; the remote TCP path (feature 013, T030) suppresses that
/// event — the reconnect loop owns the link-drop decision — and captures the
/// server-assigned window id so a reconnect re-claims THAT window.
struct ReadTaskConfig {
    /// Emit [`UiEvent::ServerDisconnected`] when the read half ends. True for the
    /// local socket; false for the remote link (the reconnect loop decides).
    report_disconnect: bool,
    /// Remote-only: capture the assigned window id from `Welcome` (T018/T030).
    assigned_window: Option<Arc<Mutex<Option<WindowId>>>>,
}

/// Drive the read half: forward server messages to the winit event loop.
/// Generic over the transport so the local Unix socket and the remote TCP path
/// (feature 013) share one loop; `config` carries the two per-transport deltas.
async fn run_read_task<R>(mut reader: R, proxy: EventLoopProxy<UiEvent>, config: ReadTaskConfig)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        match read_message::<ServerMessage, _>(&mut reader).await {
            Ok(message) => {
                // Remote path only: capture the assigned window id so a reconnect
                // re-claims THAT window even when the initial dial created a fresh
                // one (`window_id: None`, T018).
                if let Some(assigned_window) = &config.assigned_window {
                    capture_assigned_window(&message, assigned_window);
                }
                dispatch_server_message(&proxy, message);
            }
            Err(e) => {
                if config.report_disconnect {
                    tracing::warn!(error = %e, "server read error; closing connection");
                    send_event(&proxy, UiEvent::ServerDisconnected);
                } else {
                    // The reconnect loop owns the link-drop decision (T030), so
                    // do NOT emit `ServerDisconnected` here.
                    tracing::warn!(error = %e, "remote server read error; link lost");
                }
                break;
            }
        }
    }
}

fn dispatch_server_message(proxy: &EventLoopProxy<UiEvent>, message: ServerMessage) {
    let message = dispatch_session_message(proxy, message);
    let message = dispatch_workspace_message(proxy, message);
    let message = dispatch_control_message(proxy, message);
    let message = dispatch_lan_approval_message(proxy, message);
    let message = dispatch_clipboard_message(proxy, message);

    if let Some(message) = message {
        tracing::debug!(?message, "unhandled server message");
    }
}

/// Spec 010 C5: route the three new OSC 52 server variants into the
/// matching `UiEvent`s so the main thread can drive the dialog, the host
/// clipboard bridge, and the read-reply path.
fn dispatch_clipboard_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: Option<ServerMessage>,
) -> Option<ServerMessage> {
    match message? {
        ServerMessage::ClipboardPromptRequest {
            session_id,
            request_id,
            op,
            selection,
            preview,
        } => {
            send_event(
                proxy,
                UiEvent::ClipboardPromptRequest { session_id, request_id, op, selection, preview },
            );
            None
        }
        ServerMessage::ClipboardBridgeWrite { session_id, selection, payload } => {
            send_event(proxy, UiEvent::ClipboardBridgeWrite { session_id, selection, payload });
            None
        }
        ServerMessage::ClipboardBridgeReadRequest { session_id, request_id, selection } => {
            send_event(
                proxy,
                UiEvent::ClipboardBridgeReadRequest { session_id, request_id, selection },
            );
            None
        }
        other => Some(other),
    }
}

fn dispatch_session_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: ServerMessage,
) -> Option<ServerMessage> {
    match message {
        ServerMessage::PtyOutput { session_id, data } => {
            send_event(proxy, UiEvent::PtyOutput { session_id, data });
            None
        }
        ServerMessage::SessionExited { session_id, exit_code } => {
            tracing::info!(session = %session_id, ?exit_code, "session exited");
            send_event(proxy, UiEvent::SessionExited { session_id });
            None
        }
        ServerMessage::ScreenSnapshot { session_id, snapshot } => {
            send_event(proxy, UiEvent::ScreenSnapshot { session_id, snapshot });
            None
        }
        ServerMessage::SessionReplay { session_id, replay } => {
            send_event(proxy, UiEvent::SessionReplay { session_id, replay });
            None
        }
        ServerMessage::SessionCreated { session_id, workspace_id, shell_name } => {
            tracing::debug!(session = %session_id, %workspace_id, "session created via server response");
            send_event(proxy, UiEvent::SessionCreated { session_id, shell_name });
            None
        }
        ServerMessage::SearchResults { session_id, query, matches } => {
            send_event(proxy, UiEvent::SearchResults { session_id, query, matches });
            None
        }
        ServerMessage::TrimScrollback { session_id, history_rows } => {
            send_event(proxy, UiEvent::TrimScrollback { session_id, history_rows });
            None
        }
        ServerMessage::ScrollBottom { session_id } => {
            send_event(proxy, UiEvent::ScrollBottom { session_id });
            None
        }
        other => dispatch_session_metadata_message(proxy, other),
    }
}

/// Per-session metadata events (AI state, OSC title/cwd/context updates,
/// prompt marks, provider task labels, git branch). Split out of
/// `dispatch_session_message` to keep each routing function focused on a
/// single category of message.
fn dispatch_session_metadata_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: ServerMessage,
) -> Option<ServerMessage> {
    match message {
        ServerMessage::AiStateChanged { session_id, ai_state } => {
            send_event(proxy, UiEvent::AiStateChanged { session_id, ai_state });
            None
        }
        ServerMessage::AiStateCleared { session_id } => {
            send_event(proxy, UiEvent::AiStateCleared { session_id });
            None
        }
        ServerMessage::Bell { session_id } => {
            send_event(proxy, UiEvent::Bell { session_id });
            None
        }
        ServerMessage::CwdChanged { session_id, cwd } => {
            send_event(proxy, UiEvent::CwdChanged { session_id, cwd });
            None
        }
        ServerMessage::EnvStatus { session_id, state } => {
            send_event(proxy, UiEvent::EnvStatus { session_id, state });
            None
        }
        ServerMessage::SessionContextChanged { session_id, context } => {
            send_event(proxy, UiEvent::SessionContextChanged { session_id, context });
            None
        }
        ServerMessage::TitleChanged { session_id, title } => {
            send_event(proxy, UiEvent::TitleChanged { session_id, title });
            None
        }
        ServerMessage::CodexTaskLabelChanged { session_id, task_label } => {
            send_event(
                proxy,
                UiEvent::TaskLabelChanged {
                    session_id,
                    provider: AiProvider::CodexCode,
                    task_label,
                },
            );
            None
        }
        ServerMessage::CodexTaskLabelCleared { session_id } => {
            send_event(
                proxy,
                UiEvent::TaskLabelCleared { session_id, provider: AiProvider::CodexCode },
            );
            None
        }
        ServerMessage::TaskLabelChanged { session_id, provider, task_label } => {
            send_event(proxy, UiEvent::TaskLabelChanged { session_id, provider, task_label });
            None
        }
        ServerMessage::TaskLabelCleared { session_id, provider } => {
            send_event(proxy, UiEvent::TaskLabelCleared { session_id, provider });
            None
        }
        ServerMessage::PromptReceived { session_id, provider, text } => {
            tracing::debug!(session = %session_id, ?provider, "prompt received");
            send_event(proxy, UiEvent::PromptReceived { session_id, text });
            None
        }
        ServerMessage::GitBranch { session_id, branch } => {
            send_event(proxy, UiEvent::GitBranch { session_id, branch });
            None
        }
        ServerMessage::PromptMark { session_id, kind, click_events, exit_code } => {
            tracing::debug!(session = %session_id, ?exit_code, "prompt mark received");
            send_event(proxy, UiEvent::PromptMark { session_id, kind, click_events, exit_code });
            None
        }
        other => Some(other),
    }
}

fn dispatch_workspace_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: Option<ServerMessage>,
) -> Option<ServerMessage> {
    match message? {
        ServerMessage::WorkspaceInfo {
            workspace_id,
            name,
            accent_color,
            split_direction,
            project_root,
        } => {
            send_event(
                proxy,
                UiEvent::WorkspaceInfo {
                    workspace_id,
                    name,
                    accent_color,
                    split_direction,
                    project_root,
                },
            );
            None
        }
        ServerMessage::SessionList { sessions, workspace_tree, workspaces } => {
            send_event(proxy, UiEvent::SessionList { sessions, workspace_tree, workspaces });
            None
        }
        ServerMessage::WorkspaceNamed { workspace_id, name, project_root } => {
            send_event(proxy, UiEvent::WorkspaceNamed { workspace_id, name, project_root });
            None
        }
        ServerMessage::WorkspaceNotesSnapshot { collections } => {
            send_event(proxy, UiEvent::WorkspaceNotesSnapshot { collections });
            None
        }
        ServerMessage::WorkspaceNotesChanged { collection } => {
            send_event(proxy, UiEvent::WorkspaceNotesChanged { collection });
            None
        }
        ServerMessage::Welcome { window_id, other_windows, clipboard_gating, participant_id } => {
            tracing::info!(%window_id, others = other_windows.len(), clipboard_gating, "received Welcome");
            send_event(
                proxy,
                UiEvent::ClipboardGatingNegotiated { server_supports: clipboard_gating },
            );
            send_event(proxy, UiEvent::Welcome { window_id, other_windows, participant_id });
            None
        }
        ServerMessage::WindowClosed { window_id } => {
            tracing::info!(%window_id, "received WindowClosed from server");
            send_event(proxy, UiEvent::WindowClosed { window_id });
            None
        }
        other => Some(other),
    }
}

fn dispatch_control_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: Option<ServerMessage>,
) -> Option<ServerMessage> {
    match message? {
        ServerMessage::QuitRequested => {
            tracing::info!("received QuitRequested from server");
            send_event(proxy, UiEvent::QuitRequested);
            None
        }
        ServerMessage::RunAction { action } => {
            tracing::info!(?action, "received RunAction from server");
            send_event(proxy, UiEvent::RunAction { action });
            None
        }
        ServerMessage::ActionDispatched { window_id } => {
            tracing::debug!(%window_id, "ignoring ActionDispatched on UI client connection");
            None
        }
        ServerMessage::Error { message } => {
            tracing::warn!(%message, "server error");
            send_event(proxy, UiEvent::ServerError { message });
            None
        }
        ServerMessage::UpdateAvailable { version, release_url } => {
            tracing::info!(%version, "update available");
            tracing::debug!(%release_url, "update release URL");
            send_event(proxy, UiEvent::UpdateAvailable { version });
            None
        }
        ServerMessage::UpdateProgress { state } => {
            send_event(proxy, UiEvent::UpdateProgress { state });
            None
        }
        ServerMessage::WindowTakenOver { device_name, login_name } => {
            tracing::info!(
                %device_name,
                %login_name,
                "received WindowTakenOver; this client is now displaced"
            );
            send_event(proxy, UiEvent::WindowTakenOver { device_name, login_name });
            None
        }
        ServerMessage::RemoteDisconnect { reason } => {
            tracing::info!(?reason, "received RemoteDisconnect sever notice from remote server");
            send_event(proxy, UiEvent::RemoteSevered { reason });
            None
        }
        ServerMessage::RemotePeerList { peers } => {
            tracing::debug!(count = peers.len(), "received RemotePeerList from local server");
            send_event(proxy, UiEvent::RemotePeerList { peers });
            None
        }
        ServerMessage::LanPeerList { peers } => {
            tracing::debug!(count = peers.len(), "received LanPeerList from local server");
            send_event(proxy, UiEvent::LanPeerList { peers });
            None
        }
        ServerMessage::WindowList { windows } => {
            // Feature 013 (T022): the local server's window list, polled by the
            // owning-machine status bar to surface remote-controlled windows. The
            // connect picker's probe reads its `WindowList` on a dedicated
            // connection, so this arm only ever sees the local-server reply.
            tracing::debug!(count = windows.len(), "received WindowList from local server");
            send_event(proxy, UiEvent::LocalWindowList { windows });
            None
        }
        other => dispatch_share_message(proxy, other),
    }
}

/// Feature 015 (T015/T020): route the v3 share-roster / control-transfer server
/// variants into their matching `UiEvent`s. Split out of
/// [`dispatch_control_message`] so each routing function stays within the
/// per-function complexity budget.
fn dispatch_share_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: ServerMessage,
) -> Option<ServerMessage> {
    match message {
        ServerMessage::ShareRoster { window_id, participants, mode, holder } => {
            tracing::debug!(%window_id, count = participants.len(), ?mode, ?holder, "ShareRoster");
            send_event(proxy, UiEvent::ShareRoster { window_id, participants, mode, holder });
            None
        }
        ServerMessage::ControlRequested { window_id, from } => {
            tracing::info!(%window_id, requester = %from.device_name, "received ControlRequested");
            send_event(proxy, UiEvent::ControlRequested { window_id, from });
            None
        }
        ServerMessage::ControlDenied { window_id } => {
            tracing::info!(%window_id, "received ControlDenied");
            send_event(proxy, UiEvent::ControlDenied { window_id });
            None
        }
        ServerMessage::ShareEnded { window_id, reason } => {
            tracing::info!(%window_id, ?reason, "received ShareEnded");
            send_event(proxy, UiEvent::ShareEnded { window_id, reason });
            None
        }
        other => Some(other),
    }
}

/// Feature 014 (T018): route the owning-side LAN device-approval push
/// ([`ServerMessage::LanApprovalRequest`], local socket only) into
/// [`UiEvent::LanApprovalRequest`] so the main thread can raise the approval
/// prompt. Kept out of [`dispatch_control_message`] so that routing function
/// stays within its line budget.
fn dispatch_lan_approval_message(
    proxy: &EventLoopProxy<UiEvent>,
    message: Option<ServerMessage>,
) -> Option<ServerMessage> {
    match message? {
        ServerMessage::LanApprovalRequest {
            request_id,
            device_name,
            fingerprint_words,
            network_label,
            name_collision,
        } => {
            tracing::info!(
                request_id,
                %device_name,
                %network_label,
                name_collision,
                "received LanApprovalRequest from local server; raising approval prompt"
            );
            send_event(
                proxy,
                UiEvent::LanApprovalRequest {
                    request_id,
                    device_name,
                    fingerprint_words,
                    network_label,
                    name_collision,
                },
            );
            None
        }
        other => Some(other),
    }
}

/// Drive the write half: receive commands from the main thread and forward
/// them to the server. Generic over the transport so the local Unix socket and
/// the remote TCP path (feature 013) share one loop.
async fn run_write_task<W>(
    mut writer: W,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ClientCommand>>>,
    proxy: EventLoopProxy<UiEvent>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        // Clone the Arc so the spawn_blocking closure owns its reference.
        let rx_clone = Arc::<Mutex<mpsc::Receiver<ClientCommand>>>::clone(&cmd_rx);

        // Bridge the blocking recv() call into async.
        let recv_result = tokio::task::spawn_blocking(move || {
            rx_clone.lock().map_err(|_| ()).and_then(|guard| guard.recv().map_err(|_| ()))
        })
        .await;

        let Ok(Ok(cmd)) = recv_result else {
            // Sender dropped, mutex poisoned, or JoinError -- channel closed.
            break;
        };

        let msg = command_to_message(cmd);

        if let Err(e) = write_message(&mut writer, &msg).await {
            tracing::warn!(error = %e, "server write error; closing connection");
            send_event(&proxy, UiEvent::ServerDisconnected);
            break;
        }
    }

    // Best-effort flush before dropping the writer.
    if let Err(e) = writer.flush().await {
        tracing::debug!(error = %e, "flush on write task exit failed");
    }
}

/// Convert a `ClientCommand` to a `ClientMessage` for the wire.
fn command_to_message(cmd: ClientCommand) -> ClientMessage {
    match cmd {
        ClientCommand::KeyInput { session_id, data, dismisses_attention } => {
            ClientMessage::KeyInput { session_id, data, dismisses_attention }
        }
        ClientCommand::Resize { session_id, size } => ClientMessage::Resize { session_id, size },
        ClientCommand::CreateSession {
            workspace_id,
            split_direction,
            cwd,
            size,
            command,
            env_envelope_id,
        } => ClientMessage::CreateSession {
            workspace_id,
            split_direction,
            cwd,
            size,
            command,
            env_envelope_id,
        },
        ClientCommand::CloseSession { session_id } => ClientMessage::CloseSession { session_id },
        ClientCommand::Subscribe { session_ids } => ClientMessage::Subscribe { session_ids },
        ClientCommand::ListSessions => ClientMessage::ListSessions,
        ClientCommand::AttachSessions { session_ids, dimensions } => {
            ClientMessage::AttachSessions { session_ids, dimensions }
        }
        ClientCommand::ConfigReloaded => ClientMessage::ConfigReloaded,
        ClientCommand::ReportWorkspaceTree { tree } => ClientMessage::ReportWorkspaceTree { tree },
        ClientCommand::Hello { window_id, takeover } => {
            // A local-socket Hello is non-displacing (`takeover = false`) for
            // every ordinary launch. The one exception is a displaced client
            // reclaiming its window (feature 013, T017/T026), spawned with the
            // `--reclaim` CLI flag so its first Hello carries `takeover = true`
            // and the local server swaps the writer back. The remote dial path
            // sets `takeover` directly in `remote_ipc_main`.
            ClientMessage::Hello { window_id, clipboard_gating: true, takeover }
        }
        ClientCommand::CloseWindow { window_id } => ClientMessage::CloseWindow { window_id },
        ClientCommand::QuitAll => ClientMessage::QuitAll,
        ClientCommand::TriggerUpdate => ClientMessage::TriggerUpdate,
        ClientCommand::DismissUpdate => ClientMessage::DismissUpdate,
        ClientCommand::FocusChanged { gained, lost } => {
            ClientMessage::FocusChanged { gained, lost }
        }
        ClientCommand::SearchRequest { session_id, query, limit } => {
            ClientMessage::SearchRequest { session_id, query, limit }
        }
        ClientCommand::WorkspaceNotesGet { workspace_ids } => {
            ClientMessage::WorkspaceNotesGet { workspace_ids }
        }
        ClientCommand::WorkspaceNotesMutate { mutation } => {
            ClientMessage::WorkspaceNotesMutate { mutation }
        }
        ClientCommand::ClipboardPromptResponse { request_id, decision } => {
            ClientMessage::ClipboardPromptResponse { request_id, decision }
        }
        ClientCommand::ClipboardBridgeReadReply { request_id, payload } => {
            ClientMessage::ClipboardBridgeReadReply { request_id, payload }
        }
        ClientCommand::ListRemotePeers => ClientMessage::ListRemotePeers,
        ClientCommand::ListWindows => ClientMessage::ListWindows,
        ClientCommand::ListLanPeers => ClientMessage::ListLanPeers,
        ClientCommand::LanApprovalDecision { request_id, approve } => {
            ClientMessage::LanApprovalDecision { request_id, approve }
        }
        ClientCommand::ControlClaim { window_id } => ClientMessage::ControlClaim { window_id },
        ClientCommand::ControlGrant { window_id, participant_id, accept } => {
            ClientMessage::ControlGrant { window_id, participant_id, accept }
        }
    }
}

/// Async entry point running on the background thread's Tokio runtime.
///
/// Connects to the server and then drives the read and write halves
/// concurrently until the connection is closed.
///
/// Session creation is initiated by the UI thread via `ClientCommand::CreateSession`
/// rather than during the IPC handshake, ensuring exactly one session per pane.
/// Maximum time to wait for the server to become ready after starting the service.
const SERVER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum time to wait for a hot-reloaded macOS server to take over.
#[cfg(target_os = "macos")]
const SERVER_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Interval between connection retry attempts while waiting for the service.
const SERVER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Start the scribe-server process.
///
/// On Linux, uses the systemd user service. On macOS, spawns the binary
/// directly as a detached background process.
fn start_server() -> Result<(), String> {
    platform_start_server()
}

fn installed_binary_path(binary_name: &str) -> PathBuf {
    std::env::current_exe()
        .map_or_else(|_| PathBuf::from(binary_name), |exe| exe.with_file_name(binary_name))
}

fn listed_process_pids(process_name: &str) -> Result<Vec<u32>, String> {
    let output = std::process::Command::new("pgrep")
        .args(["-x", process_name])
        .output()
        .map_err(|e| format!("failed to run pgrep for {process_name}: {e}"))?;

    if !output.status.success() {
        return if output.status.code() == Some(1) {
            Ok(Vec::new())
        } else {
            Err(format!("pgrep -x {process_name} exited with {}", output.status))
        };
    }

    Ok(output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let line = std::str::from_utf8(line).ok()?.trim();
            (!line.is_empty()).then(|| line.parse::<u32>().ok()).flatten()
        })
        .collect::<Vec<_>>())
}

fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn process_is_zombie(pid: u32) -> bool {
    let path = format!("/proc/{pid}/status");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|status| {
            status.lines().find(|line| line.starts_with("State:")).map(str::to_owned)
        })
        .is_some_and(|line| line.split_whitespace().nth(1) == Some("Z"))
}

#[cfg(not(target_os = "linux"))]
fn process_is_zombie(_pid: u32) -> bool {
    false
}

fn wait_for_process_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !process_is_alive(pid) || process_is_zombie(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !process_is_alive(pid) || process_is_zombie(pid)
}

fn wait_for_tracked_clients_to_exit(client_pids: &[u32]) -> Result<(), String> {
    let mut survivors = Vec::new();
    for pid in client_pids {
        if !wait_for_process_exit(*pid, std::time::Duration::from_secs(15)) {
            survivors.push(*pid);
        }
    }
    if !survivors.is_empty() {
        return Err(format!(
            "old client processes did not exit after server restart: {survivors:?}"
        ));
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    Ok(())
}

fn spawn_replacement_client(client_exe: &Path) -> Result<(), String> {
    let child = std::process::Command::new(client_exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to relaunch client {}: {e}", client_exe.display()))?;

    tracing::info!(
        pid = child.id(),
        exe = %client_exe.display(),
        "spawned replacement client for deferred update restart"
    );
    Ok(())
}

fn wait_for_server_ready(timeout: std::time::Duration) -> Result<(), String> {
    let socket_path = server_socket_path();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "scribe-server did not become ready within {}s after deferred restart",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
fn linux_server_processes_running(uid: &str, server_exe: &str) -> Result<bool, String> {
    let status = std::process::Command::new("pgrep")
        .args(["-u", uid, "-f", server_exe])
        .status()
        .map_err(|e| format!("failed to run pgrep for server processes: {e}"))?;
    Ok(status.success())
}

#[cfg(target_os = "linux")]
fn perform_linux_update_restart(client_exe: &Path) -> Result<(), String> {
    let identity = current_identity();
    let service_name = identity.systemd_service_name();
    let server_exe = client_exe.with_file_name(identity.server_binary_name());
    let server_exe_str = server_exe.to_string_lossy().into_owned();
    let uid = scribe_common::socket::current_uid().to_string();

    sync_linux_service_environment();

    drop(std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status());
    drop(std::process::Command::new("systemctl").args(["--user", "stop", service_name]).status());

    drop(
        std::process::Command::new("pkill")
            .args(["-u", uid.as_str(), "-f", server_exe_str.as_str()])
            .status(),
    );

    for _ in 0..10 {
        if !linux_server_processes_running(uid.as_str(), server_exe_str.as_str())? {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if linux_server_processes_running(uid.as_str(), server_exe_str.as_str())? {
        drop(
            std::process::Command::new("pkill")
                .args(["-9", "-u", uid.as_str(), "-f", server_exe_str.as_str()])
                .status(),
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    drop(std::fs::remove_file(server_socket_path()));
    drop(std::fs::remove_file(handoff_socket_path()));

    drop(
        std::process::Command::new("systemctl")
            .args(["--user", "reset-failed", service_name])
            .status(),
    );

    platform_start_server()?;
    wait_for_server_ready(SERVER_STARTUP_TIMEOUT)
}

#[cfg(target_os = "macos")]
fn terminate_pid(pid: i32, label: &str) -> Result<(), String> {
    let pid_str = pid.to_string();
    let status = std::process::Command::new("kill")
        .arg(pid_str.as_str())
        .status()
        .map_err(|e| format!("failed to signal {label} pid {pid}: {e}"))?;
    if !status.success() {
        return Err(format!("kill {pid} for {label} exited with {status}"));
    }

    if wait_for_process_exit(pid as u32, std::time::Duration::from_secs(5)) {
        return Ok(());
    }

    let status = std::process::Command::new("kill")
        .args(["-9", pid_str.as_str()])
        .status()
        .map_err(|e| format!("failed to force-kill {label} pid {pid}: {e}"))?;
    if !status.success() {
        return Err(format!("kill -9 {pid} for {label} exited with {status}"));
    }

    if wait_for_process_exit(pid as u32, std::time::Duration::from_secs(1)) {
        Ok(())
    } else {
        Err(format!("timed out waiting for {label} pid {pid} to exit"))
    }
}

#[cfg(target_os = "macos")]
fn perform_macos_update_restart() -> Result<(), String> {
    // Find every scribe-server process other than ourselves and terminate them.
    // pgrep is the canonical signal because querying the IPC socket can fail
    // when the old server's accept loop is wedged (process alive but ECONNREFUSED) —
    // exactly the case this fallback exists to recover from.
    let identity = current_identity();
    let current_pid = std::process::id();
    let server_pids = listed_process_pids(identity.server_binary_name())?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect::<Vec<_>>();

    for pid in &server_pids {
        if let Err(e) = terminate_pid(*pid as i32, "scribe-server") {
            tracing::warn!(pid = *pid, "failed to terminate stale scribe-server: {e}");
        }
    }

    let _ = std::fs::remove_file(server_socket_path());
    let _ = std::fs::remove_file(handoff_socket_path());

    // Use `start_server_via_launchctl(true)` (kickstart -k) rather than
    // `platform_start_server()` so launchctl unconditionally kills any
    // KeepAlive-respawned instance and starts a fresh one. With `false`,
    // kickstart is a no-op when launchctl has already restarted the dead
    // service between our `terminate_pid` calls and now — leaving a server
    // whose socket file we just removed.
    start_server_via_launchctl(true).or_else(|e| {
        tracing::warn!("launchctl kickstart -k failed ({e}), falling back to direct spawn");
        start_server_directly(false)
    })?;
    wait_for_server_ready(SERVER_STARTUP_TIMEOUT)
}

/// Finish a deferred update that needs a true cold restart.
///
/// The helper captures the currently running client PIDs, restarts the server
/// out-of-process, waits for the old windows to exit and flush restore state,
/// then launches a single fresh client that will claim the restore snapshot
/// and fan out additional windows as needed.
pub fn finish_update_restart() -> Result<(), String> {
    let identity = current_identity();
    let client_exe = installed_binary_path(identity.client_binary_name());
    let current_pid = std::process::id();
    let client_pids = listed_process_pids(identity.client_binary_name())?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect::<Vec<_>>();

    #[cfg(target_os = "linux")]
    perform_linux_update_restart(&client_exe)?;

    #[cfg(target_os = "macos")]
    perform_macos_update_restart()?;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(String::from("deferred update restart is only supported on macOS and Linux"));

    wait_for_tracked_clients_to_exit(&client_pids)?;
    spawn_replacement_client(&client_exe)
}

#[cfg(target_os = "linux")]
fn platform_start_server() -> Result<(), String> {
    sync_linux_service_environment();
    let identity = current_identity();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", identity.systemd_service_name()])
        .status()
        .map_err(|e| format!("failed to run systemctl: {e}"))?;
    if status.success() {
        tracing::info!(service = identity.systemd_service_name(), "server service started");
        Ok(())
    } else {
        Err(format!("systemctl start exited with {status}"))
    }
}

#[cfg(target_os = "linux")]
fn sync_linux_service_environment() {
    const GUI_ENV_VARS: &[&str] = &[
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "XAUTHORITY",
    ];

    let present: Vec<&str> =
        GUI_ENV_VARS.iter().copied().filter(|name| std::env::var_os(name).is_some()).collect();
    if !present.is_empty() {
        match std::process::Command::new("systemctl")
            .arg("--user")
            .arg("import-environment")
            .args(&present)
            .status()
        {
            Ok(status) if status.success() => {
                tracing::debug!(vars = ?present, "refreshed user systemd GUI environment");
            }
            Ok(status) => {
                tracing::warn!(vars = ?present, %status, "systemctl import-environment failed");
            }
            Err(e) => {
                tracing::warn!(vars = ?present, "failed to run systemctl import-environment: {e}");
            }
        }
    }

    let missing: Vec<&str> =
        GUI_ENV_VARS.iter().copied().filter(|name| std::env::var_os(name).is_none()).collect();
    if !missing.is_empty() {
        match std::process::Command::new("systemctl")
            .arg("--user")
            .arg("unset-environment")
            .args(&missing)
            .status()
        {
            Ok(status) if status.success() => {
                tracing::debug!(vars = ?missing, "cleared absent GUI vars from user systemd env");
            }
            Ok(status) => {
                tracing::warn!(vars = ?missing, %status, "systemctl unset-environment failed");
            }
            Err(e) => {
                tracing::warn!(vars = ?missing, "failed to run systemctl unset-environment: {e}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_start_server() -> Result<(), String> {
    match start_server_via_launchctl(false) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("launchctl start failed ({e}), falling back to direct spawn");
            start_server_directly(false)
        }
    }
}

#[cfg(target_os = "macos")]
fn restart_server() -> Result<(), String> {
    // Prefer a direct `--upgrade` spawn so the new server performs a handoff
    // with the still-running old one instead of racing it on `server.lock`.
    // `launchctl kickstart -k` only kills the old server when launchd is the
    // one managing it; after a DMG drop-replace the old server becomes a
    // launchd orphan, and kickstart silently starts a non-upgrade child that
    // crash-loops on the flock the orphan still holds.
    match start_server_directly(true) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("--upgrade spawn failed ({e}), falling back to launchctl kickstart");
            start_server_via_launchctl(true)
        }
    }
}

#[cfg(target_os = "macos")]
fn start_server_via_launchctl(force_restart: bool) -> Result<(), String> {
    let identity = current_identity();
    let uid = scribe_common::socket::current_uid();
    let domain = format!("user/{uid}");
    let service = format!("user/{uid}/{}", identity.launchd_label());

    let home = std::env::var("HOME").map_err(|e| format!("HOME not set: {e}"))?;
    let agents_dir = std::path::PathBuf::from(&home).join("Library/LaunchAgents");
    let installed_plist = agents_dir.join(identity.launchd_plist_name());

    std::fs::create_dir_all(&agents_dir)
        .map_err(|e| format!("failed to create LaunchAgents dir: {e}"))?;

    let server_exe = bundled_server_exe_path(identity)?;
    let plist = launchd_plist_contents(identity.launchd_label(), &server_exe);
    let refreshed = sync_launchd_plist(&installed_plist, &plist)?;

    if refreshed {
        tracing::info!(
            plist = %installed_plist.display(),
            server = %server_exe.display(),
            "updated launchd agent plist"
        );
        rebootstrap_launchd_agent(&domain, &service, &installed_plist)?;
    }

    match kickstart_launchd_agent(&service, force_restart) {
        Ok(()) => Ok(()),
        Err(e) if !refreshed => {
            tracing::warn!("launchctl kickstart failed ({e}), re-bootstrapping agent");
            rebootstrap_launchd_agent(&domain, &service, &installed_plist)?;
            kickstart_launchd_agent(&service, force_restart)
        }
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn start_server_directly(upgrade: bool) -> Result<(), String> {
    use std::process::Stdio;

    let identity = current_identity();
    // Resolve server binary relative to current executable.
    // In a .app bundle: Contents/MacOS/scribe-server
    // In dev: same directory as scribe-client
    let exe = std::env::current_exe().map_err(|e| format!("failed to get current exe: {e}"))?;
    let server_exe = exe.with_file_name(identity.server_binary_name());

    if !server_exe.exists() {
        return Err(format!("server binary not found at {}", server_exe.display()));
    }

    let mut command = std::process::Command::new(&server_exe);
    if upgrade {
        command.arg("--upgrade");
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn scribe-server: {e}"))?;

    tracing::info!(pid = child.id(), exe = %server_exe.display(), upgrade, "spawned scribe-server");
    Ok(())
}

#[cfg(target_os = "macos")]
fn bundled_server_exe_path(identity: scribe_common::app::AppIdentity) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("failed to get current exe: {e}"))?;
    let server_exe = exe.with_file_name(identity.server_binary_name());
    if server_exe.exists() {
        Ok(server_exe)
    } else {
        Err(format!("server binary not found at {}", server_exe.display()))
    }
}

#[cfg(target_os = "macos")]
fn launchd_plist_contents(label: &str, server_exe: &Path) -> String {
    let label = escape_launchd_plist_value(label);
    let server_exe = escape_launchd_plist_value(&server_exe.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>

	<key>ProgramArguments</key>
	<array>
		<string>{server_exe}</string>
	</array>

	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>

	<key>ProcessType</key>
	<string>Background</string>

	<key>ThrottleInterval</key>
	<integer>1</integer>

	<key>StandardOutPath</key>
	<string>/dev/null</string>

	<key>StandardErrorPath</key>
	<string>/dev/null</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn escape_launchd_plist_value(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn sync_launchd_plist(path: &Path, expected: &str) -> Result<bool, String> {
    let current = match std::fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("failed to read launchd plist {}: {e}", path.display())),
    };

    if current.as_deref() == Some(expected) {
        return Ok(false);
    }

    std::fs::write(path, expected)
        .map_err(|e| format!("failed to write launchd plist {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn rebootstrap_launchd_agent(domain: &str, service: &str, plist: &Path) -> Result<(), String> {
    let bootout_status =
        std::process::Command::new("launchctl").args(["bootout", service]).status();
    match bootout_status {
        Ok(status) if status.success() => {
            tracing::info!(%service, "booted out existing launchd agent");
        }
        Ok(status) => {
            tracing::debug!(%service, %status, "launchd agent bootout skipped");
        }
        Err(e) => {
            tracing::debug!(%service, "launchctl bootout unavailable: {e}");
        }
    }

    let status = std::process::Command::new("launchctl")
        .arg("bootstrap")
        .arg(domain)
        .arg(plist)
        .status()
        .map_err(|e| format!("failed to run launchctl bootstrap: {e}"))?;
    if !status.success() {
        return Err(format!("launchctl bootstrap exited with {status}"));
    }
    tracing::info!(%service, plist = %plist.display(), "bootstrapped launchd agent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn kickstart_launchd_agent(service: &str, force_restart: bool) -> Result<(), String> {
    let mut command = std::process::Command::new("launchctl");
    command.arg("kickstart");
    if force_restart {
        command.arg("-k");
    }
    let status = command
        .arg(service)
        .status()
        .map_err(|e| format!("failed to run launchctl kickstart: {e}"))?;
    if !status.success() {
        return Err(format!("launchctl kickstart exited with {status}"));
    }
    tracing::info!(%service, force_restart, "kickstarted launchd agent");
    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectedServerInfo {
    pid: i32,
    exe_path: Option<PathBuf>,
    start_time_secs: Option<u64>,
}

#[cfg(target_os = "macos")]
fn connected_server_info(stream: &tokio::net::UnixStream) -> Result<ConnectedServerInfo, String> {
    use nix::sys::socket::{getsockopt, sockopt};
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let pid = getsockopt(stream, sockopt::LocalPeerPid)
        .map_err(|e| format!("failed to query server peer pid: {e}"))?;

    let sys_pid = Pid::from(pid as usize);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::everything()
            .without_cpu()
            .without_disk_usage()
            .without_memory()
            .without_tasks()
            .with_exe(UpdateKind::Always),
    );

    let process = system.process(sys_pid);
    Ok(ConnectedServerInfo {
        pid,
        exe_path: process.and_then(|proc| proc.exe()).map(std::path::Path::to_path_buf),
        start_time_secs: process.map(sysinfo::Process::start_time),
    })
}

#[cfg(target_os = "macos")]
fn file_modified_epoch_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs())
}

#[cfg(target_os = "macos")]
fn same_file_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

#[cfg(target_os = "macos")]
fn stale_server_reason(
    running: &ConnectedServerInfo,
    bundled_server_exe: &Path,
    bundled_modified_secs: Option<u64>,
) -> Option<String> {
    if let Some(exe_path) =
        running.exe_path.as_deref().filter(|path| !same_file_path(path, bundled_server_exe))
    {
        return Some(format!(
            "running server path {} differs from installed {}",
            exe_path.display(),
            bundled_server_exe.display()
        ));
    }

    match (running.start_time_secs, bundled_modified_secs) {
        (Some(start_time), Some(modified)) if modified > start_time => Some(format!(
            "installed server binary modified at {modified} after running server started at {start_time}"
        )),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct StaleRefresh {
    old_pid: i32,
    reason: String,
}

#[cfg(target_os = "macos")]
fn refresh_stale_connected_server(
    stream: &tokio::net::UnixStream,
) -> Result<Option<StaleRefresh>, String> {
    let bundled_server = bundled_server_exe_path(current_identity())?;
    let running = connected_server_info(stream)?;
    let bundled_modified = file_modified_epoch_secs(&bundled_server);
    let Some(reason) = stale_server_reason(&running, &bundled_server, bundled_modified) else {
        return Ok(None);
    };

    tracing::info!(pid = running.pid, %reason, "connected scribe-server is stale; requesting refresh");
    restart_server()?;
    Ok(Some(StaleRefresh { old_pid: running.pid, reason }))
}

#[cfg(target_os = "macos")]
fn peer_pid_of(stream: &tokio::net::UnixStream) -> Result<i32, String> {
    use nix::sys::socket::{getsockopt, sockopt};
    getsockopt(stream, sockopt::LocalPeerPid).map_err(|e| format!("failed to query peer pid: {e}"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ConnectedServerInfo, escape_launchd_plist_value, launchd_plist_contents,
        stale_server_reason,
    };

    #[test]
    fn launchd_plist_escapes_server_path() {
        let plist = launchd_plist_contents(
            "com.scribe.server",
            Path::new("/Applications/A&B's <Scribe>.app/Contents/MacOS/scribe-server"),
        );
        assert!(plist.contains("com.scribe.server"));
        assert!(plist.contains(
            "/Applications/A&amp;B&apos;s &lt;Scribe&gt;.app/Contents/MacOS/scribe-server"
        ));
    }

    #[test]
    fn launchd_plist_value_escape_handles_xml_meta_chars() {
        assert_eq!(escape_launchd_plist_value("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
    }

    #[test]
    fn stale_server_reason_detects_path_drift() {
        let running = ConnectedServerInfo {
            pid: 42,
            exe_path: Some(PathBuf::from(
                "/Applications/Old Scribe.app/Contents/MacOS/scribe-server",
            )),
            start_time_secs: Some(100),
        };

        let reason = stale_server_reason(
            &running,
            Path::new("/Applications/Scribe.app/Contents/MacOS/scribe-server"),
            Some(100),
        );

        assert!(reason.is_some(), "expected path drift to mark server stale");
    }

    #[test]
    fn stale_server_reason_detects_newer_installed_binary() {
        let running = ConnectedServerInfo {
            pid: 42,
            exe_path: Some(PathBuf::from("/Applications/Scribe.app/Contents/MacOS/scribe-server")),
            start_time_secs: Some(100),
        };

        let reason = stale_server_reason(
            &running,
            Path::new("/Applications/Scribe.app/Contents/MacOS/scribe-server"),
            Some(101),
        );

        assert!(reason.is_some(), "expected newer installed binary to mark server stale");
    }

    #[test]
    fn stale_server_reason_ignores_matching_fresh_server() {
        let running = ConnectedServerInfo {
            pid: 42,
            exe_path: Some(PathBuf::from("/Applications/Scribe.app/Contents/MacOS/scribe-server")),
            start_time_secs: Some(101),
        };

        let reason = stale_server_reason(
            &running,
            Path::new("/Applications/Scribe.app/Contents/MacOS/scribe-server"),
            Some(100),
        );

        assert!(reason.is_none(), "matching current server should not be marked stale");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_start_server() -> Result<(), String> {
    Err(String::from("server auto-start not supported on this platform"))
}

/// Try to connect to the server socket. If the server isn't running, start it
/// and retry until it's ready or the timeout expires.
async fn connect_or_start_server(
    socket_path: &Path,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    // First attempt — server may already be running.
    if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
        #[cfg(target_os = "macos")]
        match refresh_stale_connected_server(&stream) {
            Ok(Some(refresh)) => {
                drop(stream);
                tracing::info!(
                    old_pid = refresh.old_pid,
                    reason = %refresh.reason,
                    "waiting for refreshed scribe-server"
                );
                return wait_for_refreshed_server(
                    socket_path,
                    refresh.old_pid,
                    SERVER_REFRESH_TIMEOUT,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "failed to verify running server freshness ({e}); using existing connection"
                );
            }
        }
        return Ok(stream);
    }

    tracing::info!("server not running, starting scribe-server");
    start_server().map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    wait_for_server_connection(socket_path, SERVER_STARTUP_TIMEOUT).await
}

/// Wait for the refreshed server to take over after a hot-reload was requested.
///
/// Polls the socket until the connected peer's PID differs from the supplied
/// `old_pid`, which signals the new server has bound the socket. If `timeout`
/// elapses without a takeover — usually because the `--upgrade` child failed
/// internally (e.g. handoff version mismatch) and the old server is still
/// holding the lock and socket — fall back to `perform_macos_update_restart`,
/// which force-terminates the old PID, clears stale lock/socket files, and
/// starts a fresh server. Sessions in the old server are lost, but Scribe
/// launches instead of timing out and crashing.
#[cfg(target_os = "macos")]
async fn wait_for_refreshed_server(
    socket_path: &Path,
    old_pid: i32,
    timeout: std::time::Duration,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            match peer_pid_of(&stream) {
                Ok(pid) if pid != old_pid => {
                    tracing::info!(new_pid = pid, "connected to refreshed scribe-server");
                    return Ok(stream);
                }
                Ok(_) => {
                    // Old server still owns the socket — handoff hasn't taken
                    // over yet. Keep polling until the deadline.
                    drop(stream);
                }
                Err(e) => {
                    tracing::warn!(
                        "could not query peer pid after refresh ({e}); accepting connection"
                    );
                    return Ok(stream);
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                old_pid,
                "handoff did not take over within {}s; falling back to cold restart",
                timeout.as_secs()
            );
            perform_macos_update_restart().map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("cold-restart fallback after handoff timeout failed: {e}").into()
                },
            )?;
            // perform_macos_update_restart already waited for the new server to
            // become ready, so a single direct connect should succeed. Skip
            // wait_for_server_connection so its own timeout-fallback can't try
            // to cold-restart the server we just spawned.
            return tokio::net::UnixStream::connect(socket_path).await.map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("connect after handoff cold-restart failed: {e}").into()
                },
            );
        }
    }
}

async fn wait_for_server_connection(
    socket_path: &Path,
    timeout: std::time::Duration,
) -> Result<tokio::net::UnixStream, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;

        if let Ok(stream) = tokio::net::UnixStream::connect(socket_path).await {
            #[cfg(target_os = "macos")]
            match refresh_stale_connected_server(&stream) {
                Ok(Some(refresh)) => {
                    tracing::info!(
                        old_pid = refresh.old_pid,
                        reason = %refresh.reason,
                        "stale server reconnected during wait; switching to refresh-wait"
                    );
                    drop(stream);
                    return Box::pin(wait_for_refreshed_server(
                        socket_path,
                        refresh.old_pid,
                        SERVER_REFRESH_TIMEOUT,
                    ))
                    .await;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        "failed to verify running server freshness while waiting ({e}); using existing connection"
                    );
                }
            }
            tracing::info!("connected to scribe-server");
            return Ok(stream);
        }

        if tokio::time::Instant::now() >= deadline {
            #[cfg(target_os = "macos")]
            if let Some(stream) = try_cold_restart_recovery(socket_path).await? {
                return Ok(stream);
            }
            return Err(format!(
                "scribe-server did not become ready within {}s",
                timeout.as_secs()
            )
            .into());
        }
    }
}

/// Last-ditch recovery when `wait_for_server_connection` times out.
///
/// Covers the case where a stuck old scribe-server is alive enough to hold
/// `server.lock` (blocking fresh starts) but its IPC accept loop has died
/// (so `connect()` returns ECONNREFUSED). In that state `refresh_stale_connected_server`
/// never fires — the initial connect fails before the staleness check can run —
/// and the spawned fresh server crashes on `flock` while the client polls in
/// vain. Resolution: `pgrep` for any non-current scribe-server, terminate
/// them, then run `perform_macos_update_restart` to clear sockets and start a
/// fresh server. Returns `None` when no stale processes exist so legitimate
/// startup timeouts still surface as errors.
#[cfg(target_os = "macos")]
async fn try_cold_restart_recovery(
    socket_path: &Path,
) -> Result<Option<tokio::net::UnixStream>, Box<dyn std::error::Error + Send + Sync>> {
    let identity = current_identity();
    let current_pid = std::process::id();
    let stale: Vec<u32> = listed_process_pids(identity.server_binary_name())
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?
        .into_iter()
        .filter(|pid| *pid != current_pid)
        .collect();

    if stale.is_empty() {
        return Ok(None);
    }

    tracing::warn!(
        ?stale,
        "scribe-server connect timed out with stale server processes still alive; forcing cold restart"
    );
    perform_macos_update_restart().map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("cold-restart recovery after connect timeout failed: {e}").into()
    })?;

    let stream = tokio::net::UnixStream::connect(socket_path).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("connect after cold-restart recovery failed: {e}").into()
        },
    )?;
    Ok(Some(stream))
}

/// Maximum time to wait for a remote TCP connect before declaring the peer
/// unreachable. Bounds the FR-004 combined connection-failure outcome so a
/// black-holed tailnet address cannot hang the connect flow indefinitely.
const REMOTE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Dial a remote scribe-server's tailnet address. Connect-only: no server
/// auto-start, staleness refresh, or upgrade handling (contrast
/// [`connect_or_start_server`]). A refused connect or an elapsed
/// [`REMOTE_CONNECT_TIMEOUT`] both surface as an `Err`, which the caller maps to
/// [`RemoteConnectOutcome::ConnectionFailure`].
async fn connect_remote(host: &str, port: u16) -> std::io::Result<tokio::net::TcpStream> {
    let connect = tokio::net::TcpStream::connect((host, port));
    let stream = match tokio::time::timeout(REMOTE_CONNECT_TIMEOUT, connect).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("remote connect to {host}:{port} timed out"),
            ));
        }
    };
    // Interactive keystroke traffic must not wait on Nagle coalescing; match the
    // responsiveness of the local socket. Best-effort — a failure here does not
    // invalidate the connection.
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "failed to set TCP_NODELAY on remote connection");
    }
    Ok(stream)
}

/// Run the feature-013 remote preamble on a freshly connected TCP stream: send
/// [`ClientMessage::RemoteHandshake`] as the first frame, then read the
/// mandatory [`ServerMessage::RemoteHandshakeReply`]. Every terminal condition
/// maps to a [`RemoteConnectOutcome`]: an accepted reply, a typed refusal, or —
/// for an EOF, an I/O error, or any frame other than the reply — the merged
/// [`RemoteConnectOutcome::ConnectionFailure`] (FR-004).
async fn remote_handshake(
    stream: &mut tokio::net::TcpStream,
    device_name: String,
) -> RemoteConnectOutcome {
    let preamble = ClientMessage::RemoteHandshake {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        scribe_version: env!("CARGO_PKG_VERSION").to_owned(),
        device_name,
    };
    if let Err(error) = write_message(stream, &preamble).await {
        tracing::warn!(%error, "failed to send remote handshake preamble");
        return RemoteConnectOutcome::ConnectionFailure;
    }

    match read_message::<ServerMessage, _>(stream).await {
        Ok(ServerMessage::RemoteHandshakeReply {
            accepted,
            refusal,
            server_remote_protocol_version,
            server_scribe_version,
        }) => match (accepted, refusal) {
            (true, _) => {
                tracing::info!(
                    server_remote_protocol_version,
                    %server_scribe_version,
                    "remote handshake accepted"
                );
                RemoteConnectOutcome::Accepted
            }
            (false, Some(reason)) => {
                tracing::info!(
                    ?reason,
                    server_remote_protocol_version,
                    %server_scribe_version,
                    "remote handshake refused"
                );
                RemoteConnectOutcome::Refused(reason)
            }
            (false, None) => {
                // A refusal with no reason is a protocol violation; treat it as
                // a generic connection failure rather than inventing a cause.
                tracing::warn!("remote handshake refused without a reason");
                RemoteConnectOutcome::ConnectionFailure
            }
        },
        Ok(other) => {
            tracing::warn!(
                ?other,
                "unexpected first frame from remote server; expected RemoteHandshakeReply"
            );
            RemoteConnectOutcome::ConnectionFailure
        }
        Err(error) => {
            tracing::warn!(%error, "remote server closed before handshake reply");
            RemoteConnectOutcome::ConnectionFailure
        }
    }
}

/// Async entry point for a REMOTE (TCP) connection, on the background thread's
/// Tokio runtime (feature 013, T009).
///
/// Connect-only: dials `host:port`, runs the remote preamble, reports the typed
/// outcome to the UI, and — only on acceptance — sends `Hello` and drives the
/// same read/write task loop as [`ipc_main`]. It never starts or upgrades the
/// peer's server.
/// Poll granularity for the command bridge — bounds how quickly it notices the
/// UI dropped its sender (window closing) so the IPC thread can wind down.
const REMOTE_BRIDGE_POLL: Duration = Duration::from_millis(200);

/// Base / cap for the remote auto-reconnect backoff, and the attempt ceiling
/// after which the loop settles into the combined connection-failure state
/// (feature 013, T030 / research D6). Capped exponential: attempt `n` waits
/// `min(BASE * 2^(n-1), CAP)`.
const RECONNECT_BACKOFF_BASE_MS: u64 = 500;
const RECONNECT_BACKOFF_CAP_MS: u64 = 15_000;
const RECONNECT_MAX_ATTEMPTS: u32 = 8;
/// Granularity of the cancelable backoff wait — how promptly a Cancel / sever
/// stops the loop while it is sleeping between attempts.
const RECONNECT_CANCEL_POLL: Duration = Duration::from_millis(150);

/// How one connected remote session ended, so the reconnect loop can tell a
/// recoverable link drop from a deliberate UI shutdown (feature 013, T030).
enum RemoteSessionEnd {
    /// The transport failed (read or write error) — retry with backoff.
    LinkLost,
    /// The command channel closed (UI window closing) — stop for good.
    Stopped,
}

/// Forward the std command channel into an async one for the whole remote
/// lifetime (feature 013, T030). A single blocking task owns the receiver, so
/// commands survive reconnects and no per-attempt task is ever left blocked in
/// `recv` holding it. Ends when the UI drops its sender (window closing) or
/// `remote_ipc_main` drops the async receiver (loop finished).
fn spawn_command_bridge(
    cmd_rx: mpsc::Receiver<ClientCommand>,
    async_tx: tokio::sync::mpsc::UnboundedSender<ClientCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while !async_tx.is_closed() {
            let cmd = match cmd_rx.recv_timeout(REMOTE_BRIDGE_POLL) {
                Ok(cmd) => cmd,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if async_tx.send(cmd).is_err() {
                break;
            }
        }
    })
}

/// Drop the async command receiver so the bridge notices and exits, then await
/// its join to wind the IPC thread down cleanly (feature 013, T030).
async fn shutdown_command_bridge(
    async_rx: tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    bridge: tokio::task::JoinHandle<()>,
) {
    drop(async_rx);
    if let Err(error) = bridge.await {
        tracing::debug!(%error, "remote command bridge join failed");
    }
}

/// Capture the server-assigned window id from a `Welcome` so a reconnect can
/// re-claim the SAME window (feature 013, T030).
fn capture_assigned_window(message: &ServerMessage, assigned_window: &Mutex<Option<WindowId>>) {
    if let ServerMessage::Welcome { window_id, .. } = message
        && let Ok(mut slot) = assigned_window.lock()
    {
        *slot = Some(*window_id);
    }
}

/// Drive one live remote (tailnet TCP) session until the link drops or the UI
/// closes the command channel (feature 013, T030). Splits the stream into owned
/// halves and delegates to the transport-agnostic [`run_split_session`] so the
/// tailnet and LAN (feature 014, T015) paths share one read/write loop.
async fn run_remote_session(
    stream: tokio::net::TcpStream,
    async_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    proxy: &EventLoopProxy<UiEvent>,
    assigned_window: &Arc<Mutex<Option<WindowId>>>,
) -> RemoteSessionEnd {
    let (reader, writer) = stream.into_split();
    run_split_session(reader, writer, async_rx, proxy, assigned_window).await
}

/// Drive one live remote session over already-split read/write halves — the
/// shared core of both the tailnet TCP path ([`run_remote_session`]) and the LAN
/// mutual-TLS path ([`run_lan_session`], feature 014, T015). Generic over the
/// transport so an established LAN connection behaves byte-identically to a
/// tailnet one after the preamble.
///
/// The read half runs on its own task (so `read_message` is never cancelled
/// mid-frame — it is not cancel-safe); the write half drains the async command
/// channel inline. `async_rx` is borrowed, not moved, so it is reused across
/// reconnect attempts. The read config suppresses `ServerDisconnected` (the
/// reconnect loop owns the link-drop decision) and captures the assigned window
/// id for reconnect re-claim.
async fn run_split_session<R, W>(
    reader: R,
    mut writer: W,
    async_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    proxy: &EventLoopProxy<UiEvent>,
    assigned_window: &Arc<Mutex<Option<WindowId>>>,
) -> RemoteSessionEnd
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    // `JoinHandle` is `Unpin`, so `&mut read_task` is a pollable future across
    // loop iterations without pinning (mirrors the local `ipc_main` join).
    let mut read_task = tokio::spawn(run_read_task(
        reader,
        proxy.clone(),
        ReadTaskConfig {
            report_disconnect: false,
            assigned_window: Some(Arc::clone(assigned_window)),
        },
    ));

    loop {
        tokio::select! {
            _ = &mut read_task => {
                // The read half ended — the server closed the link.
                return RemoteSessionEnd::LinkLost;
            }
            cmd = async_rx.recv() => {
                let Some(cmd) = cmd else {
                    // The command bridge closed: the UI dropped its sender, i.e.
                    // this window is shutting down. Stop for good.
                    read_task.abort();
                    return RemoteSessionEnd::Stopped;
                };
                let msg = command_to_message(cmd);
                // The write completes here (not in a select arm), so it is never
                // cancelled mid-frame the way a `read_message` arm would be.
                if let Err(error) = write_message(&mut writer, &msg).await {
                    tracing::warn!(%error, "remote server write error; link lost");
                    read_task.abort();
                    return RemoteSessionEnd::LinkLost;
                }
            }
        }
    }
}

/// Run the initial remote connect + preamble + `Hello`, preserving the exact
/// T009/T014 reporting (`RemoteDialOutcome`, and `ServerDisconnected` if the
/// accepted link fails on the first `Hello`). Returns the live stream on
/// success, or `None` when the dial failed / was refused / the Hello write
/// failed — each already surfaced to the UI.
async fn connect_and_attach_initial(
    host: &str,
    port: u16,
    window_id: Option<WindowId>,
    takeover: bool,
    proxy: &EventLoopProxy<UiEvent>,
) -> Option<tokio::net::TcpStream> {
    let mut stream = match connect_remote(host, port).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%host, port, %error, "remote dial failed");
            send_event(
                proxy,
                UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
            );
            return None;
        }
    };

    let outcome = remote_handshake(&mut stream, crate::read_hostname()).await;
    send_event(proxy, UiEvent::RemoteDialOutcome { outcome });
    if outcome != RemoteConnectOutcome::Accepted {
        // Refused or ConnectionFailure: terminal. No session traffic.
        return None;
    }

    let hello = ClientMessage::Hello { window_id, clipboard_gating: true, takeover };
    if let Err(error) = write_message(&mut stream, &hello).await {
        tracing::warn!(%error, "failed to send Hello on accepted remote connection");
        send_event(proxy, UiEvent::ServerDisconnected);
        return None;
    }
    Some(stream)
}

/// Capped exponential backoff delay for reconnect `attempt` (1-based).
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let millis =
        RECONNECT_BACKOFF_BASE_MS.saturating_mul(1u64 << shift).min(RECONNECT_BACKOFF_CAP_MS);
    Duration::from_millis(millis)
}

/// Sleep `delay`, polling the cancel switch on a fixed cadence. Returns `true`
/// if cancellation fired (caller stops), `false` if the full delay elapsed.
async fn backoff_or_cancel(delay: Duration, cancel: &RemoteReconnectCancel) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if cancel.is_cancelled() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(RECONNECT_CANCEL_POLL)).await;
    }
}

/// Resolve once the reconnect cancel switch fires, polling on the shared
/// [`RECONNECT_CANCEL_POLL`] cadence — the same idiom [`backoff_or_cancel`] uses.
/// Lets an in-flight connect attempt be raced to a prompt stop so a Cancel (or
/// delivered sever) abandons it rather than letting it complete and go live
/// (FR-011).
async fn wait_for_cancel(cancel: &RemoteReconnectCancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(RECONNECT_CANCEL_POLL).await;
    }
}

/// Outcome of one in-flight reconnect attempt, kept separate from the live
/// stream so [`reconnect_with_backoff`] can race the whole attempt against a
/// Cancel and only emit `RemoteReconnected` once cancellation is ruled out
/// (FR-011).
enum ReconnectAttempt {
    /// Handshake accepted and `Hello { takeover: false }` sent — the live stream.
    Attached(tokio::net::TcpStream),
    /// The peer refused authoritatively (e.g. disabled, version) — settle.
    Refused(RemoteRefusal),
    /// Connect / handshake / `Hello` failed — retry with backoff.
    Failed,
}

/// Run one reconnect attempt: connect, run the preamble, and re-claim the SAME
/// window with `Hello { takeover: false }` — the auto-reconnect path NEVER seizes
/// control (FR-011). Emits NO UI events, so [`reconnect_with_backoff`] can
/// discard it on Cancel (dropping the half-open stream) without a spurious
/// `RemoteReconnected`; the caller decides whether the attempt goes live.
async fn try_reconnect_attempt(
    host: &str,
    port: u16,
    window_id: Option<WindowId>,
    attempt: u32,
) -> ReconnectAttempt {
    let mut stream = match connect_remote(host, port).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::debug!(%host, port, attempt, %error, "remote reconnect attempt failed");
            return ReconnectAttempt::Failed;
        }
    };
    match remote_handshake(&mut stream, crate::read_hostname()).await {
        RemoteConnectOutcome::Accepted => {
            let hello = ClientMessage::Hello { window_id, clipboard_gating: true, takeover: false };
            match write_message(&mut stream, &hello).await {
                Ok(()) => ReconnectAttempt::Attached(stream),
                Err(error) => {
                    tracing::warn!(%error, attempt, "reconnect Hello write failed");
                    ReconnectAttempt::Failed
                }
            }
        }
        RemoteConnectOutcome::Refused(reason) => ReconnectAttempt::Refused(reason),
        RemoteConnectOutcome::ConnectionFailure => ReconnectAttempt::Failed,
    }
}

/// Retry the remote link with capped exponential backoff until it re-attaches,
/// the user cancels, the peer refuses, or the attempt ceiling is hit
/// (feature 013, T030). On success it re-sends `Hello { takeover: false }` — the
/// auto-reconnect path NEVER seizes control (FR-011); an explicit reclaim is the
/// only takeover. Returns the live stream, or `None` when the loop should end
/// (the matching terminal UI event was already emitted, except a plain user
/// Cancel, which the UI settles itself).
async fn reconnect_with_backoff(
    host: &str,
    port: u16,
    window_id: Option<WindowId>,
    proxy: &EventLoopProxy<UiEvent>,
    cancel: &RemoteReconnectCancel,
) -> Option<tokio::net::TcpStream> {
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        attempt += 1;
        send_event(proxy, UiEvent::RemoteReconnecting { attempt });
        if backoff_or_cancel(backoff_delay(attempt), cancel).await {
            return None;
        }
        if cancel.is_cancelled() {
            return None;
        }

        // Race the whole connect → handshake → `Hello` sequence against the
        // cancel switch. A Cancel (or delivered sever) fired while the attempt is
        // in flight must abandon it — the `select!` drops the attempt future and
        // its half-open stream — instead of completing and emitting
        // `RemoteReconnected` to go live over a settled overlay (FR-011: Cancel
        // settles into a disconnected state). Dropping mid-frame is safe here
        // precisely because the stream is discarded, never reused.
        let outcome = tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => return None,
            outcome = try_reconnect_attempt(host, port, window_id, attempt) => outcome,
        };

        match outcome {
            ReconnectAttempt::Attached(stream) => {
                // A Cancel that raced the attempt's completion still wins: never
                // revive a window the user settled (belt-and-suspenders with the
                // UI's `is_settled` guard on `RemoteReconnected`).
                if cancel.is_cancelled() {
                    return None;
                }
                send_event(proxy, UiEvent::RemoteReconnected);
                return Some(stream);
            }
            ReconnectAttempt::Refused(reason) => {
                // An authoritative refusal on reconnect (e.g. remote access
                // disabled, version mismatch) is terminal — reuse the sever path
                // so the UI shows the reason's copy and settles rather than
                // retrying a listener that is up but refusing us.
                tracing::info!(?reason, "remote reconnect refused; settling");
                send_event(proxy, UiEvent::RemoteSevered { reason });
                return None;
            }
            ReconnectAttempt::Failed => {}
        }

        if attempt >= RECONNECT_MAX_ATTEMPTS {
            // Give up auto-retrying and settle into the combined
            // connection-failure copy (offline / not running / disabled). The
            // UI's one-action reconnect starts a fresh attempt.
            send_event(proxy, UiEvent::RemoteReconnectFailed);
            return None;
        }
    }
}

/// Async entry point for a REMOTE (TCP) connection, on the background thread's
/// Tokio runtime (feature 013, T009/T030).
///
/// Connect-only: dials `host:port`, runs the remote preamble, reports the typed
/// outcome to the UI, and — only on acceptance — sends `Hello` and drives one
/// live session. When an established session's link drops, it auto-reconnects
/// with capped exponential backoff (T030), re-claiming the SAME window with
/// `Hello { takeover: false }` (never seizing control). A clean command-channel
/// close (window closing), a user Cancel, an authoritative sever, or an
/// exhausted backoff ends the thread. It never starts or upgrades the peer's
/// server.
async fn remote_ipc_main(
    proxy: EventLoopProxy<UiEvent>,
    cmd_rx: mpsc::Receiver<ClientCommand>,
    dial: RemoteDial,
    cancel: RemoteReconnectCancel,
) {
    let RemoteDial { host, port, window_id, takeover } = dial;

    // Bridge the std command channel to an async one for the whole (multi-attempt)
    // lifetime, so every (re)connect drains commands cancel-safely (T030).
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
    let bridge = spawn_command_bridge(cmd_rx, async_tx);

    // The window a reconnect re-claims: the server-assigned id captured from the
    // first `Welcome`, falling back to the dial target. Shared with the read task.
    let assigned_window = Arc::new(Mutex::new(window_id));

    // Initial attempt — preserves the exact T009/T014 dial reporting.
    let Some(stream) = connect_and_attach_initial(&host, port, window_id, takeover, &proxy).await
    else {
        shutdown_command_bridge(async_rx, bridge).await;
        return;
    };

    let mut end = run_remote_session(stream, &mut async_rx, &proxy, &assigned_window).await;

    // Auto-reconnect loop: a dropped link is retried; a clean channel close (UI
    // window closing) or a settled cancel/sever ends the thread.
    loop {
        if matches!(end, RemoteSessionEnd::Stopped) || cancel.is_cancelled() {
            break;
        }
        let reclaim_window = assigned_window.lock().ok().and_then(|slot| *slot).or(window_id);
        match reconnect_with_backoff(&host, port, reclaim_window, &proxy, &cancel).await {
            Some(next_stream) => {
                end =
                    run_remote_session(next_stream, &mut async_rx, &proxy, &assigned_window).await;
            }
            None => break,
        }
    }

    shutdown_command_bridge(async_rx, bridge).await;
}

// ── Feature 014 (T015): LAN dial over mutual TLS ─────────────────────────────
//
// The LAN transport reuses every transport-agnostic piece of the 013 remote
// path — the command bridge, the read task, `run_split_session`, the capped
// backoff/cancel helpers, and the reconnect UI events — and adds only the
// LAN-specific dial: TCP → pinned mutual TLS → `LanHello` preamble → the owning
// side's device-approval gate. An established LAN connection therefore behaves
// byte-identically to a tailnet one after `Hello`.

/// The dialer-side encrypted LAN stream once the mutual-TLS handshake completes.
type LanStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Bundled parameters for a LAN dial: the peer's resolved LAN subnet address +
/// LAN port, and the window claim to make once the approval gate admits us. The
/// LAN analogue of [`RemoteDial`].
pub struct LanDial {
    host: String,
    port: u16,
    /// `None` creates a fresh window on the peer; `Some` claims an existing one.
    window_id: Option<WindowId>,
    /// Set only for explicit picker attach / lost-control reclaim, never on the
    /// auto-reconnect path (FR-011).
    takeover: bool,
}

/// Start the IPC client against a LAN scribe-server over mutual TLS, on a
/// background thread (feature 014, T015).
///
/// The LAN analogue of [`start_remote_ipc_thread`]: same dedicated `std::thread`
/// owning a single-threaded Tokio runtime, and the same read/write task loop once
/// the link is established ([`run_split_session`]), but it dials `host:port` on
/// the peer's LAN subnet address, wraps the TCP stream in pinned mutual TLS
/// ([`LanTls`]), runs the `LanHello` preamble + owning-side device-approval gate,
/// and only then sends `Hello`. Strictly connect-only — like the tailnet path it
/// NEVER starts, refreshes, or upgrades the peer's server. The dial result, the
/// interim "held pending approval" state, and any reconnect surface to the UI as
/// [`UiEvent::LanDialOutcome`] / [`UiEvent::LanAwaitingApproval`] / the reused
/// reconnect events; once accepted every other event is identical to a tailnet
/// session.
///
/// FOLLOW-UP — does the client need its own device-identity module? Effectively
/// yes. This reuses the SERVER-owned `scribe_server::lan::{identity, tls}` by
/// depending on the whole `scribe-server` crate: DRY (no duplicated SPKI-pinning
/// verifier, signature delegation, or keyring sealing) and it gives the dialer a
/// STABLE `device_id` across dials — the basis for remembered approval (US2) —
/// but it is architecturally heavy and inverts the client→server layering.
/// Recommended follow-up (do not block T015): extract `lan::{identity, tls}` (+
/// the `env_store::keystore` wrapper + `DevicePins` / `DeviceId`) into a shared
/// crate (e.g. `scribe-lan`) so the client links only the LAN identity/TLS
/// surface, and decide client key provisioning — direct keyring load (as here)
/// vs. a local-socket `GetLanIdentity`-style query to this machine's own server,
/// and whether the connecting side may MINT the identity or must only LOAD one
/// the (interactive, keyring-backed) owning side already generated.
pub fn start_lan_ipc_thread(
    proxy: EventLoopProxy<UiEvent>,
    dial: LanDial,
    cancel: RemoteReconnectCancel,
) -> mpsc::Sender<ClientCommand> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    // As on the tailnet path, `Hello` is NOT pre-queued: the `LanHello` preamble
    // + approval gate must precede it, and `lan_ipc_main` owns that ordering,
    // draining commands over one bridge for the whole (multi-attempt) lifetime.
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(error = %error, "failed to create tokio runtime for LAN IPC");
                send_event(
                    &proxy,
                    UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
                );
                return;
            }
        };
        rt.block_on(lan_ipc_main(proxy, cmd_rx, dial, cancel));
    });
    cmd_tx
}

/// The connecting side keeps no trusted-*server* pin store in v1: device approval
/// and trust are owning-side (the LAN listener gates the dialer, not the reverse),
/// so the dialer's reused [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier)
/// only has to encrypt the link and prove the peer holds its key (the delegated,
/// never-stubbed handshake-signature check in `scribe_server::lan::tls`). This pin
/// set therefore classifies every server as first-seen; the classification is
/// recorded by the verifier but unused on the dial side.
struct NoServerPins;

impl DevicePins for NoServerPins {
    fn is_pinned(&self, _device_id: &DeviceId) -> bool {
        false
    }
}

/// Build the mutual-TLS dialer that presents this machine's persistent device
/// identity behind the SPKI-pinning verifier (feature 014, T015). The identity is
/// fetched from this machine's co-located `scribe-server` over the local socket
/// ([`fetch_lan_dial_identity`]) rather than read from the OS keyring directly: the
/// keyring-sealed key is granted only to the creating binary (`scribe-server`) by a
/// per-item ACL on macOS, so this different binary (`scribe-client`) cannot read it.
/// The client rebuilds a [`DeviceIdentity`] from the returned DER via
/// [`identity::DeviceIdentity::from_der`], so the connecting side still presents a
/// STABLE `device_id` (remembered approval, US2) and the security-critical verifier
/// stays in one place ([`LanTls`]). Fails closed (server down / identity
/// unavailable) exactly as the owning side does; the caller maps that to a
/// `ConnectionFailure` outcome.
async fn build_lan_tls() -> Result<LanTls, identity::IdentityError> {
    let (cert_der, key_pkcs8_der) = fetch_lan_dial_identity().await?;
    let device_identity = identity::DeviceIdentity::from_der(cert_der, key_pkcs8_der)?;
    Ok(LanTls::new(Arc::new(device_identity), Arc::new(NoServerPins)))
}

/// Fetch this machine's OWN LAN device identity (public certificate DER + sealed
/// `PKCS#8` private-key DER) from its co-located `scribe-server` over the local
/// Unix socket, rather than reading the OS keyring directly from this binary. On
/// macOS the keyring (legacy `SecKeychain`) grants the sealed device key by a
/// per-item ACL that trusts ONLY the creating binary (`scribe-server`); a different
/// binary (`scribe-client`) is denied (errSecInteractionNotAllowed) with no usable
/// prompt, so a direct `identity::load_or_generate()` here fails BEFORE any TCP even
/// on a reachable peer. The server is the sole keychain accessor and serves the
/// identity over a local-socket-only [`ClientMessage::GetLanDialIdentity`] first
/// frame (refused over any remote transport). Used on ALL platforms for uniformity.
/// Fails closed — returns an [`identity::IdentityError`] the caller maps to a
/// `ConnectionFailure` outcome — on any transport error or an `available = false`
/// reply, so the dial never proceeds without a valid identity.
async fn fetch_lan_dial_identity() -> Result<(Vec<u8>, Vec<u8>), identity::IdentityError> {
    let socket_path = server_socket_path();
    let mut stream = tokio::net::UnixStream::connect(&socket_path).await.map_err(|error| {
        identity::IdentityError::LocalIdentityUnavailable(format!(
            "connecting to the local server socket failed: {error}"
        ))
    })?;
    write_message(&mut stream, &ClientMessage::GetLanDialIdentity).await.map_err(|error| {
        identity::IdentityError::LocalIdentityUnavailable(format!(
            "sending GetLanDialIdentity to the local server failed: {error}"
        ))
    })?;
    // Read until the identity reply arrives, ignoring any interleaved frames; the
    // connection drops as soon as we have it.
    loop {
        match read_message::<ServerMessage, _>(&mut stream).await {
            Ok(ServerMessage::LanDialIdentity { available, cert_der, private_key_pkcs8_der }) => {
                if !available || cert_der.is_empty() || private_key_pkcs8_der.is_empty() {
                    return Err(identity::IdentityError::LocalIdentityUnavailable(
                        "the local server reported no LAN device identity available".to_owned(),
                    ));
                }
                return Ok((cert_der, private_key_pkcs8_der));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(identity::IdentityError::LocalIdentityUnavailable(format!(
                    "the local server closed before the LAN dial identity arrived: {error}"
                )));
            }
        }
    }
}

/// The invariant target of a LAN dial for the whole (multi-attempt) lifetime: the
/// pinned mutual-TLS dialer plus the peer's LAN address. Grouped so the dial /
/// reconnect helpers stay within the argument budget and the target travels as
/// one unit (mirrors how [`RemoteDial`] groups the tailnet target).
struct LanDialer {
    lan_tls: LanTls,
    host: String,
    port: u16,
}

/// Dial the peer over TCP (reusing [`connect_remote`]'s timeout + `TCP_NODELAY`)
/// and complete the pinned mutual-TLS handshake. Returns the encrypted stream, or
/// `None` on any connect / handshake failure (both merge into the connecting
/// side's `ConnectionFailure` outcome, FR-004).
async fn lan_tls_connect(dialer: &LanDialer) -> Option<LanStream> {
    let tcp = match connect_remote(&dialer.host, dialer.port).await {
        Ok(tcp) => tcp,
        Err(error) => {
            tracing::warn!(host = %dialer.host, port = dialer.port, %error, "LAN TCP dial failed");
            return None;
        }
    };
    match dialer.lan_tls.connect(tcp).await {
        Ok((stream, _peer)) => {
            tracing::info!(host = %dialer.host, port = dialer.port, "LAN mutual TLS established");
            Some(stream)
        }
        Err(error) => {
            tracing::warn!(
                host = %dialer.host,
                port = dialer.port,
                %error,
                "LAN mutual TLS handshake failed"
            );
            None
        }
    }
}

/// Run the LAN preamble on a freshly established mutual-TLS stream: send
/// [`ClientMessage::LanHello`], then read the owning side's approval-gate frames
/// until a terminal [`ServerMessage::LanApprovalResult`]. An unknown device is
/// first told it is waiting ([`ServerMessage::LanApprovalPending`]) — surfaced via
/// `pending_sink` as [`UiEvent::LanAwaitingApproval`] — and the read blocks (no
/// timeout of our own) until the owning user decides or the peer's approval hold
/// times out; an already-trusted device is admitted straight to
/// `LanApprovalResult { approved: true }` with no pending frame. Every terminal
/// condition maps to a [`LanConnectOutcome`]: accepted, a typed [`LanRefusal`], or
/// — for an EOF / I/O error / unexpected frame / reason-less refusal — the merged
/// [`LanConnectOutcome::ConnectionFailure`]. `pending_sink` is `None` on the
/// auto-reconnect path (a background reattach never raises the overlay; a trusted
/// device never gets a pending frame anyway).
async fn lan_handshake<S>(
    stream: &mut S,
    device_name: String,
    pending_sink: Option<&EventLoopProxy<UiEvent>>,
) -> LanConnectOutcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let preamble =
        ClientMessage::LanHello { device_name, remote_protocol_version: REMOTE_PROTOCOL_VERSION };
    if let Err(error) = write_message(stream, &preamble).await {
        tracing::warn!(%error, "failed to send LAN hello preamble");
        return LanConnectOutcome::ConnectionFailure;
    }

    loop {
        match read_message::<ServerMessage, _>(stream).await {
            Ok(ServerMessage::LanApprovalPending) => {
                tracing::info!("LAN connection held pending device approval on peer");
                if let Some(proxy) = pending_sink {
                    send_event(proxy, UiEvent::LanAwaitingApproval);
                }
                // Keep reading: the terminal `LanApprovalResult` follows the owning
                // user's decision (or the peer's approval timeout).
            }
            Ok(ServerMessage::LanApprovalResult { approved: true, .. }) => {
                tracing::info!("LAN device approval accepted");
                return LanConnectOutcome::Accepted;
            }
            Ok(ServerMessage::LanApprovalResult { approved: false, refusal: Some(reason) }) => {
                tracing::info!(?reason, "LAN device approval refused");
                return LanConnectOutcome::Refused(reason);
            }
            Ok(ServerMessage::LanApprovalResult { approved: false, refusal: None }) => {
                // A refusal with no reason is a protocol violation; treat it as a
                // generic connection failure rather than inventing a cause.
                tracing::warn!("LAN approval refused without a reason");
                return LanConnectOutcome::ConnectionFailure;
            }
            Ok(other) => {
                tracing::warn!(
                    ?other,
                    "unexpected frame during LAN approval; expected LanApprovalResult"
                );
                return LanConnectOutcome::ConnectionFailure;
            }
            Err(error) => {
                tracing::warn!(%error, "LAN peer closed before an approval result");
                return LanConnectOutcome::ConnectionFailure;
            }
        }
    }
}

/// Run the initial LAN connect + TLS + `LanHello` + approval gate + `Hello`,
/// reporting the typed [`UiEvent::LanDialOutcome`] exactly once. Returns the live
/// stream on acceptance, or `None` when the dial failed / was refused / the Hello
/// write failed / the user cancelled — each already surfaced to the UI.
///
/// Unlike the tailnet initial dial, the `LanHello`/approval exchange is raced
/// against a Cancel: the owning user's decision can take up to the peer's approval
/// timeout, so the "Waiting for approval…" overlay is cancelable. A Cancel drops
/// the half-open TLS stream and emits no outcome (the UI settles the cancel
/// itself, mirroring the reconnect race, FR-011).
async fn connect_and_attach_lan_initial(
    dialer: &LanDialer,
    window_id: Option<WindowId>,
    takeover: bool,
    proxy: &EventLoopProxy<UiEvent>,
    cancel: &RemoteReconnectCancel,
) -> Option<LanStream> {
    let Some(mut stream) = lan_tls_connect(dialer).await else {
        send_event(
            proxy,
            UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
        );
        return None;
    };

    let outcome = tokio::select! {
        biased;
        () = wait_for_cancel(cancel) => return None,
        outcome = lan_handshake(&mut stream, crate::read_hostname(), Some(proxy)) => outcome,
    };
    send_event(proxy, UiEvent::LanDialOutcome { outcome });
    if outcome != LanConnectOutcome::Accepted {
        // Refused or ConnectionFailure: terminal. No session traffic.
        return None;
    }

    let hello = ClientMessage::Hello { window_id, clipboard_gating: true, takeover };
    if let Err(error) = write_message(&mut stream, &hello).await {
        tracing::warn!(%error, "failed to send Hello on accepted LAN connection");
        send_event(proxy, UiEvent::ServerDisconnected);
        return None;
    }
    Some(stream)
}

/// Drive one live LAN session: split the TLS stream into read/write halves and
/// delegate to the shared [`run_split_session`] core (feature 014, T015). The LAN
/// analogue of [`run_remote_session`].
async fn run_lan_session(
    stream: LanStream,
    async_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    proxy: &EventLoopProxy<UiEvent>,
    assigned_window: &Arc<Mutex<Option<WindowId>>>,
) -> RemoteSessionEnd {
    let (reader, writer) = tokio::io::split(stream);
    run_split_session(reader, writer, async_rx, proxy, assigned_window).await
}

/// Outcome of one in-flight LAN reconnect attempt, kept separate from the live
/// stream so [`lan_reconnect_with_backoff`] can race the whole attempt against a
/// Cancel. The LAN analogue of [`ReconnectAttempt`].
enum LanReconnectAttempt {
    /// Approved (already-trusted device) and `Hello { takeover: false }` sent.
    /// Boxed because an established [`LanStream`] (a `rustls` connection state) is
    /// far larger than the other variants (`clippy::large_enum_variant`).
    Attached(Box<LanStream>),
    /// The peer refused authoritatively (LAN disabled, revoked, version) — settle.
    Refused(LanRefusal),
    /// Connect / TLS / handshake / `Hello` failed — retry with backoff.
    Failed,
}

/// Run one LAN reconnect attempt: connect + TLS + `LanHello`, then re-claim the
/// SAME window with `Hello { takeover: false }` — the auto-reconnect path NEVER
/// seizes control (FR-011). Emits NO UI events (`pending_sink` is `None`) so
/// [`lan_reconnect_with_backoff`] can discard it on Cancel. A device that
/// connected once is already trusted, so the approval gate admits it without a
/// pending hold. The LAN analogue of [`try_reconnect_attempt`].
async fn try_lan_reconnect_attempt(
    dialer: &LanDialer,
    window_id: Option<WindowId>,
    attempt: u32,
) -> LanReconnectAttempt {
    let Some(mut stream) = lan_tls_connect(dialer).await else {
        tracing::debug!(
            host = %dialer.host,
            port = dialer.port,
            attempt,
            "LAN reconnect attempt failed to establish TLS"
        );
        return LanReconnectAttempt::Failed;
    };
    match lan_handshake(&mut stream, crate::read_hostname(), None).await {
        LanConnectOutcome::Accepted => {
            let hello = ClientMessage::Hello { window_id, clipboard_gating: true, takeover: false };
            match write_message(&mut stream, &hello).await {
                Ok(()) => LanReconnectAttempt::Attached(Box::new(stream)),
                Err(error) => {
                    tracing::warn!(%error, attempt, "LAN reconnect Hello write failed");
                    LanReconnectAttempt::Failed
                }
            }
        }
        LanConnectOutcome::Refused(reason) => LanReconnectAttempt::Refused(reason),
        LanConnectOutcome::ConnectionFailure => LanReconnectAttempt::Failed,
    }
}

/// Retry the LAN link with capped exponential backoff until it re-attaches, the
/// user cancels, the peer refuses, or the attempt ceiling is hit (feature 014,
/// T015). Reuses the transport-agnostic backoff / cancel helpers and reconnect UI
/// events; the LAN analogue of [`reconnect_with_backoff`].
///
/// An authoritative refusal on reconnect settles into the combined
/// connection-failure state ([`UiEvent::RemoteReconnectFailed`]) rather than
/// spinning against a listener that is refusing us. The precise typed
/// [`LanRefusal`] copy is surfaced on the INITIAL dial's `LanDialOutcome`; the
/// reconnect path reuses the shared settle so it needs no separate LAN
/// sever-event plumbing (that UX wiring lands with the picker/pending work,
/// T014/T019).
async fn lan_reconnect_with_backoff(
    dialer: &LanDialer,
    window_id: Option<WindowId>,
    proxy: &EventLoopProxy<UiEvent>,
    cancel: &RemoteReconnectCancel,
) -> Option<LanStream> {
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        attempt += 1;
        send_event(proxy, UiEvent::RemoteReconnecting { attempt });
        if backoff_or_cancel(backoff_delay(attempt), cancel).await {
            return None;
        }
        if cancel.is_cancelled() {
            return None;
        }

        // Race the whole connect → TLS → handshake → `Hello` sequence against the
        // cancel switch: a Cancel that fires while an attempt is in flight drops
        // it (and its half-open stream) instead of completing and going live over
        // a settled overlay (FR-011).
        let outcome = tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => return None,
            outcome = try_lan_reconnect_attempt(dialer, window_id, attempt) => outcome,
        };

        match outcome {
            LanReconnectAttempt::Attached(stream) => {
                if cancel.is_cancelled() {
                    return None;
                }
                send_event(proxy, UiEvent::RemoteReconnected);
                return Some(*stream);
            }
            LanReconnectAttempt::Refused(reason) => {
                tracing::info!(?reason, "LAN reconnect refused; settling");
                send_event(proxy, UiEvent::RemoteReconnectFailed);
                return None;
            }
            LanReconnectAttempt::Failed => {}
        }

        if attempt >= RECONNECT_MAX_ATTEMPTS {
            send_event(proxy, UiEvent::RemoteReconnectFailed);
            return None;
        }
    }
}

/// Async entry point for a LAN (mutual-TLS) connection, on the background
/// thread's Tokio runtime (feature 014, T015). The LAN analogue of
/// [`remote_ipc_main`].
///
/// Loads this machine's device identity + builds the pinned TLS dialer (failing
/// closed to a `ConnectionFailure` outcome if the keyring is unavailable), dials
/// `host:port`, runs the `LanHello` preamble + approval gate, reports the typed
/// outcome, and — only on acceptance — sends `Hello` and drives one live session.
/// When an established session's link drops it auto-reconnects with capped
/// backoff, re-claiming the SAME window with `Hello { takeover: false }` (never
/// seizing control). A clean command-channel close (window closing), a user
/// Cancel, an authoritative refusal, or an exhausted backoff ends the thread. It
/// never starts or upgrades the peer's server.
async fn lan_ipc_main(
    proxy: EventLoopProxy<UiEvent>,
    cmd_rx: mpsc::Receiver<ClientCommand>,
    dial: LanDial,
    cancel: RemoteReconnectCancel,
) {
    let LanDial { host, port, window_id, takeover } = dial;

    // Build the pinned mutual-TLS dialer from this machine's device identity. A
    // fail-closed identity error (keyring unavailable / no state dir) becomes the
    // combined connection-failure outcome — the connecting side cannot present a
    // client certificate, so there is nothing to dial with.
    let lan_tls = match build_lan_tls().await {
        Ok(lan_tls) => lan_tls,
        Err(error) => {
            tracing::warn!(%error, "failed to load LAN device identity; dial failed closed");
            send_event(
                &proxy,
                UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
            );
            return;
        }
    };
    // The invariant dial target (pinned TLS dialer + LAN address) for the whole
    // (multi-attempt) lifetime; borrowed by the initial and reconnect helpers.
    let dialer = LanDialer { lan_tls, host, port };

    // Bridge the std command channel to an async one for the whole (multi-attempt)
    // lifetime, so every (re)connect drains commands cancel-safely (as 013).
    let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
    let bridge = spawn_command_bridge(cmd_rx, async_tx);

    // The window a reconnect re-claims: the server-assigned id captured from the
    // first `Welcome`, falling back to the dial target. Shared with the read task.
    let assigned_window = Arc::new(Mutex::new(window_id));

    let Some(stream) =
        connect_and_attach_lan_initial(&dialer, window_id, takeover, &proxy, &cancel).await
    else {
        shutdown_command_bridge(async_rx, bridge).await;
        return;
    };

    let mut end = run_lan_session(stream, &mut async_rx, &proxy, &assigned_window).await;

    loop {
        if matches!(end, RemoteSessionEnd::Stopped) || cancel.is_cancelled() {
            break;
        }
        let reclaim_window = assigned_window.lock().ok().and_then(|slot| *slot).or(window_id);
        match lan_reconnect_with_backoff(&dialer, reclaim_window, &proxy, &cancel).await {
            Some(next_stream) => {
                end = run_lan_session(next_stream, &mut async_rx, &proxy, &assigned_window).await;
            }
            None => break,
        }
    }

    shutdown_command_bridge(async_rx, bridge).await;
}

/// Dial a peer over TCP only to enumerate its windows for the connect picker
/// (feature 013, T014). Runs the remote preamble, sends a single `ListWindows`,
/// forwards the resulting [`ServerMessage::WindowList`] as
/// [`UiEvent::RemoteWindowList`], then drops the connection — it never claims a
/// window. Handshake refusals and connect/EOF errors surface as
/// [`UiEvent::RemoteDialOutcome`] so the picker renders the same typed copy as a
/// real attach. Runs on its own short-lived `std::thread` + Tokio runtime,
/// mirroring [`start_remote_ipc_thread`].
pub fn start_remote_list_windows_thread(proxy: EventLoopProxy<UiEvent>, host: String, port: u16) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "failed to create tokio runtime for remote window probe"
                );
                send_event(
                    &proxy,
                    UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
                );
                return;
            }
        };
        rt.block_on(remote_list_windows_main(proxy, host, port));
    });
}

/// Body of the window-list probe (feature 013, T014). See
/// [`start_remote_list_windows_thread`].
async fn remote_list_windows_main(proxy: EventLoopProxy<UiEvent>, host: String, port: u16) {
    let mut stream = match connect_remote(&host, port).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%host, port, %error, "remote window probe dial failed");
            send_event(
                &proxy,
                UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
            );
            return;
        }
    };

    let outcome = remote_handshake(&mut stream, crate::read_hostname()).await;
    if outcome != RemoteConnectOutcome::Accepted {
        // Refused or ConnectionFailure: the picker maps it to distinct copy.
        send_event(&proxy, UiEvent::RemoteDialOutcome { outcome });
        return;
    }

    if let Err(error) = write_message(&mut stream, &ClientMessage::ListWindows).await {
        tracing::warn!(%error, "failed to send ListWindows on remote window probe");
        send_event(
            &proxy,
            UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
        );
        return;
    }

    // Read until the WindowList arrives, ignoring any other frames the server
    // may interleave. The connection drops as soon as we have the list.
    loop {
        match read_message::<ServerMessage, _>(&mut stream).await {
            Ok(ServerMessage::WindowList { windows }) => {
                send_event(&proxy, UiEvent::RemoteWindowList { host, port, windows });
                return;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "remote server closed before window list arrived");
                send_event(
                    &proxy,
                    UiEvent::RemoteDialOutcome { outcome: RemoteConnectOutcome::ConnectionFailure },
                );
                return;
            }
        }
    }
}

/// Dial a LAN peer over mutual TLS only to enumerate its windows for the connect
/// picker's second step (feature 014, T014) — the LAN analogue of
/// [`start_remote_list_windows_thread`]. Runs the same TLS + `LanHello` +
/// device-approval gate as a real LAN attach ([`lan_handshake`], reusing the T015
/// dial building blocks), sends a single `ListWindows`, forwards the resulting
/// [`ServerMessage::WindowList`] as [`UiEvent::RemoteWindowList`] (transport-
/// neutral once past the gate), then drops the connection — it never claims a
/// window. A held-pending-approval hold surfaces as [`UiEvent::LanAwaitingApproval`]
/// (as on a real dial); every refusal / connect / TLS / EOF error surfaces as
/// [`UiEvent::LanDialOutcome`] so the picker renders the same typed LAN copy as a
/// real attach. Runs on its own short-lived `std::thread` + Tokio runtime.
pub fn start_lan_list_windows_thread(proxy: EventLoopProxy<UiEvent>, host: String, port: u16) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "failed to create tokio runtime for LAN window probe"
                );
                send_event(
                    &proxy,
                    UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
                );
                return;
            }
        };
        rt.block_on(lan_list_windows_main(proxy, host, port));
    });
}

/// Body of the LAN window-list probe (feature 014, T014). See
/// [`start_lan_list_windows_thread`].
async fn lan_list_windows_main(proxy: EventLoopProxy<UiEvent>, host: String, port: u16) {
    let lan_tls = match build_lan_tls().await {
        Ok(lan_tls) => lan_tls,
        Err(error) => {
            tracing::warn!(%error, "LAN window probe could not load device identity");
            send_event(
                &proxy,
                UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
            );
            return;
        }
    };
    // Clone `host` into the dialer so the original params stay available for the
    // `RemoteWindowList` reply below (they match the picker's stored dial target,
    // so `set_windows` accepts it), avoiding a shadowing rebind.
    let dialer = LanDialer { lan_tls, host: host.clone(), port };
    let Some(mut stream) = lan_tls_connect(&dialer).await else {
        send_event(
            &proxy,
            UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
        );
        return;
    };

    // Reuse the real dial's preamble + approval gate so an unknown device is held
    // pending approval here too (`pending_sink = Some`), and a refusal maps to the
    // same typed LAN copy in the picker.
    let outcome = lan_handshake(&mut stream, crate::read_hostname(), Some(&proxy)).await;
    if outcome != LanConnectOutcome::Accepted {
        send_event(&proxy, UiEvent::LanDialOutcome { outcome });
        return;
    }

    if let Err(error) = write_message(&mut stream, &ClientMessage::ListWindows).await {
        tracing::warn!(%error, "failed to send ListWindows on LAN window probe");
        send_event(
            &proxy,
            UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
        );
        return;
    }

    // Read until the WindowList arrives, ignoring any other frames the server may
    // interleave; the connection drops as soon as we have the list.
    loop {
        match read_message::<ServerMessage, _>(&mut stream).await {
            Ok(ServerMessage::WindowList { windows }) => {
                send_event(&proxy, UiEvent::RemoteWindowList { host, port, windows });
                return;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "LAN server closed before window list arrived");
                send_event(
                    &proxy,
                    UiEvent::LanDialOutcome { outcome: LanConnectOutcome::ConnectionFailure },
                );
                return;
            }
        }
    }
}

async fn ipc_main(
    proxy: EventLoopProxy<UiEvent>,
    cmd_rx: Arc<Mutex<mpsc::Receiver<ClientCommand>>>,
) {
    let socket_path = server_socket_path();

    let stream = match connect_or_start_server(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to connect to scribe server");
            send_event(&proxy, UiEvent::ServerDisconnected);
            return;
        }
    };

    let (reader, writer) = stream.into_split();

    let read_proxy = proxy.clone();
    let write_proxy = proxy.clone();
    // Local socket: report a dropped link as `ServerDisconnected`, capture no
    // window id (the local client already owns its `--window-id`).
    let read_task = tokio::spawn(run_read_task(
        reader,
        read_proxy,
        ReadTaskConfig { report_disconnect: true, assigned_window: None },
    ));
    let write_task = tokio::spawn(run_write_task(writer, cmd_rx, write_proxy));

    // When either task finishes, abort the other so the process can exit.
    // Typically the write task exits first (cmd_tx dropped when the UI
    // closes), while the read task would block forever on a still-alive
    // server socket.
    let mut read_task = read_task;
    let mut write_task = write_task;
    tokio::select! {
        _ = &mut read_task => {
            write_task.abort();
        }
        _ = &mut write_task => {
            read_task.abort();
        }
    }
}
