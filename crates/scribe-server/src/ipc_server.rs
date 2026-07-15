use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use scribe_pty::claude_picker_filter::ClaudePickerTruncationFilter;
use scribe_pty::ed3_filter::Ed3Filter;
use scribe_pty::lf_crlf_filter::LfCrlfFilter;
use tokio::io::{AsyncWriteExt as _, ReadHalf, WriteHalf};
use tokio::net::unix::UCred;
use tokio::net::{TcpListener, TcpStream, UnixListener};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};
use vte::Parser as VteParser;
use vte::ansi::Processor as AnsiProcessor;

use alacritty_terminal::grid::Dimensions as _;
#[cfg(test)]
use scribe_common::ai_state::AiProcessState;
use scribe_common::ai_state::{AiProvider, AiState};
use scribe_common::config as scribe_config;
use scribe_common::error::ScribeError;
use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::{SessionId, WindowId, WorkspaceId};
use scribe_common::protocol::{
    AutomationAction, ClientMessage, ControllerInfo, LanPeerInfo, LanRefusal, PromptMarkKind,
    REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, SearchMatch, ServerMessage,
    SessionInfo, TerminalSize, TrustedDeviceInfo, TrustedNetworkInfo, WindowInfo,
    WorkspaceListEntry, WorkspaceNotesMutation, WorkspaceTreeNode,
};
use scribe_common::screen::{ScreenCell, ScreenSnapshot};
use scribe_common::socket::current_uid;
use scribe_pty::event_listener::SessionEvent;
use scribe_pty::metadata::MetadataEvent;
use scribe_pty::osc_interceptor::OscInterceptor;

use crate::handoff::HandoffSession;
use crate::hook_ingress;
use crate::lan::discovery::{AdvertiseConfig, LanDiscovery, LanPeerHandle, local_hostname};
use crate::lan::identity::DeviceIdentity;
use crate::lan::network::{self, SharedTrustedNetworks, TrustedNetworksStore};
use crate::lan::tls::{DevicePins, LanTls, PeerIdentity, PinDecision};
use crate::lan::trust::{
    APPROVAL_TIMEOUT, ApprovalOutcome, ApprovalRequest, DeviceId, PendingApprovals,
    SharedTrustedDevices, TrustedDevicesStore, decode_device_id_hex,
};
use crate::releases::{ReleaseCatalog, ReleaseFetcher};
use crate::session_manager::{
    ManagedSession, SessionLaunchRequest, SessionManager, build_term_config, snapshot_term,
};
use crate::updater::UpdaterHandle;
use crate::workspace_manager::WorkspaceManager;
use crate::workspace_notes::WorkspaceNotesStore;

/// Buffer size for PTY reads. 64 KiB balances throughput and latency.
const PTY_READ_BUF_SIZE: usize = 64 * 1024;

/// Maximum payload size for a single `KeyInput` message. Legitimate keyboard
/// input is never more than a few dozen bytes; pastes are chunked by the client
/// to fit this limit. Capping at 4 KiB prevents a rogue client from writing
/// 16 MiB (the frame limit) to the PTY in one shot.
const MAX_KEY_INPUT_BYTES: usize = 4 * 1024;

/// Maximum simultaneous IPC client connections. Prevents a same-UID attacker
/// from exhausting memory/tasks by opening thousands of connections.
const MAX_CONNECTIONS: usize = 32;

/// Maximum number of session IDs in a single `Subscribe` message. Prevents
/// a client from holding the workspace write-lock in a tight loop.
const MAX_SUBSCRIBE_IDS: usize = 256;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PreservedAiScrollback {
    baseline_history: Option<usize>,
}

impl PreservedAiScrollback {
    fn reset(&mut self) {
        self.baseline_history = None;
    }

    fn needs_baseline(&self) -> bool {
        self.baseline_history.is_none()
    }

    fn set_baseline(&mut self, history: usize) {
        self.baseline_history = Some(history);
    }

    fn trim_target(&self, current_history: usize) -> Option<usize> {
        self.baseline_history.and_then(|baseline| (current_history > baseline).then_some(baseline))
    }
}

/// The write side of one client connection. A local Unix-socket connection
/// writes framed messages straight to the socket — byte-for-byte the pre-013
/// behavior. A remote (TCP) connection instead enqueues into a bounded
/// per-connection output queue drained by a dedicated task (feature 013 T029,
/// research D5), so a slow tailnet link can never block the fan-out hot path
/// (and thus never stall the server's authoritative `Term` or the other
/// clients). Boxing the local half lets one `SharedWriter` back either a
/// `WriteHalf<UnixStream>` or the remote enqueue handle without threading a
/// stream-type generic through every handler.
pub enum ClientSink {
    /// Local Unix-socket writer: framed messages go straight to the socket via
    /// the stream-generic `write_message`.
    Local(Box<dyn tokio::io::AsyncWrite + Send + Unpin>),
    /// Remote (TCP) enqueue handle into the bounded output queue.
    Remote(RemoteSink),
}

/// Shared writer half of a client connection.
pub type SharedWriter = Arc<Mutex<ClientSink>>;

/// Optional client writer: `Some` when a client is attached, `None` when
/// the session is detached (client disconnected). The PTY reader task
/// silently skips sends when `None`.
pub type ClientWriter = Arc<Mutex<Option<SharedWriter>>>;

/// Session IDs currently attached to a specific client connection.
pub type AttachedSessionIds = Arc<Mutex<HashSet<SessionId>>>;

/// Shared pointer from a live session to the current attached-session set for
/// its active client, if any.
pub type SessionAttachment = Arc<Mutex<Option<AttachedSessionIds>>>;

/// Server-wide registry of all running sessions. Shared across client
/// handlers and the handoff listener — sessions survive client disconnects.
pub type LiveSessionRegistry = Arc<RwLock<HashMap<SessionId, LiveSession>>>;

/// Registry of connected client windows, keyed by `WindowId`.
/// Used to broadcast `QuitRequested` to all connected clients.
pub type ConnectedClients = Arc<RwLock<HashMap<WindowId, SharedWriter>>>;

/// Per-window cache of the `clipboard_gating` capability flag advertised at
/// `ClientMessage::Hello` (spec 010 contract C7). A `true` entry means the
/// attached client understands the new OSC 52 IPC variants and the server
/// may emit them for sessions in that window; a missing or `false` entry
/// makes the server fall back to the headless deny path (research decision
/// 7). Updated under the same `connected_clients` write-lock the window
/// attaches under so the flag is always consistent with the writer slot.
pub type WindowClipboardGating = Arc<RwLock<HashMap<WindowId, bool>>>;

/// Feature 013: identity of the client currently controlling a window under the
/// single-writer ownership model. `Local` is a Unix-socket client on this
/// machine; `Remote` is an authenticated tailnet peer. Tracked per window in
/// [`WindowControllers`], re-bound under the same claim path as
/// [`ConnectedClients`] so the two never drift, and consumed by the window-list
/// controller field (FR-009b) and the `WindowTakenOver` naming (FR-007).
#[derive(Clone)]
pub enum ControllerIdentity {
    /// A local Unix-socket controller (this machine).
    Local,
    /// A remote tailnet controller, named by device + account.
    Remote { device_name: String, login_name: String },
}

impl ControllerIdentity {
    /// The window-list / picker view of this controller: `Some` only for a
    /// remote controller (a local controller needs no device/account label).
    fn to_controller_info(&self) -> Option<ControllerInfo> {
        match self {
            ControllerIdentity::Local => None,
            ControllerIdentity::Remote { device_name, login_name } => Some(ControllerInfo {
                device_name: device_name.clone(),
                login_name: login_name.clone(),
            }),
        }
    }

    /// The `WindowTakenOver` frame naming this controller as the new owner, sent
    /// to a displaced client (FR-007). A local controller is named "this
    /// machine" with no account label (the displaced peer already knows the
    /// account is its own).
    fn window_taken_over(&self) -> ServerMessage {
        match self {
            ControllerIdentity::Local => ServerMessage::WindowTakenOver {
                device_name: "this machine".to_string(),
                login_name: String::new(),
            },
            ControllerIdentity::Remote { device_name, login_name } => {
                ServerMessage::WindowTakenOver {
                    device_name: device_name.clone(),
                    login_name: login_name.clone(),
                }
            }
        }
    }

    /// Compact identity label for the feature-013 control-transition traces
    /// (T027): `local` for a Unix-socket controller, `device (login)` for a
    /// remote peer. Distinct from the canonical `REMOTE_AUDIT_TARGET` lines —
    /// this only annotates who holds a window across an ownership transition.
    fn transition_label(&self) -> Cow<'static, str> {
        match self {
            ControllerIdentity::Local => Cow::Borrowed("local"),
            ControllerIdentity::Remote { device_name, login_name } => {
                Cow::Owned(format!("{device_name} ({login_name})"))
            }
        }
    }
}

/// Feature 013: per-window controller identity, parallel to [`ConnectedClients`]
/// and keyed the same way. An entry exists while a window is connected; it is
/// re-bound atomically at takeover and dropped when the owning client detaches.
pub type WindowControllers = Arc<RwLock<HashMap<WindowId, ControllerIdentity>>>;

#[derive(Clone)]
pub struct IpcServerState {
    pub session_manager: Arc<SessionManager>,
    pub workspace_manager: Arc<RwLock<WorkspaceManager>>,
    pub live_sessions: LiveSessionRegistry,
    pub connected_clients: ConnectedClients,
    /// Spec 010 attach-time clipboard-gating capability bits, keyed by window.
    pub window_clipboard_gating: WindowClipboardGating,
    /// Feature 013: per-window controller identity (local vs remote peer), kept
    /// in lock-step with `connected_clients` so window-listing and the takeover
    /// banner can name the controlling machine (FR-007, FR-009b).
    pub window_controllers: WindowControllers,
    pub updater_handle: Arc<UpdaterHandle>,
    /// In-memory cache of GitHub releases populated lazily on the first
    /// `ListReleases` request and refreshed in the background past its TTL.
    /// See [`crate::releases`] for the cache state machine.
    pub release_catalog: Arc<Mutex<ReleaseCatalog>>,
    /// Fetcher used to refresh `release_catalog`. Production wires the real
    /// `GithubReleaseFetcher`; tests may inject deterministic stubs.
    pub release_fetcher: Arc<dyn ReleaseFetcher>,
    /// Authoritative server-owned workspace notes store.
    pub workspace_notes: Arc<Mutex<WorkspaceNotesStore>>,
    /// Server-wide registry of per-session env-store state (baselines,
    /// working deltas, status, and per-session persist schedulers). Owned
    /// here so hook ingress (`crate::hook_ingress`) and the session
    /// lifecycle paths share one source of truth. See
    /// `specs/006-persist-terminal-env/data-model.md` for the ownership
    /// rules.
    pub env_store: Arc<crate::env_store::EnvStoreState>,
    /// Feature 013/014: shared control handle for the remote-control listeners.
    /// Startup and every `ConfigReloaded` reconcile each transport's listener
    /// live via the supervisor (tailnet + LAN) to start, stop, or rebind it; the
    /// accept path consults the matching per-transport state for the disable-race
    /// refusal and the connection cap.
    pub remote_control: Arc<RemoteControl>,
}

struct ClientDispatchContext<'a> {
    server: &'a IpcServerState,
    writer: &'a SharedWriter,
    attached_ids: &'a AttachedSessionIds,
    window_id: WindowId,
    /// Whether this connection arrived over a remote transport (tailnet or LAN)
    /// rather than the local Unix socket. Gates local-only messages — e.g. the
    /// feature-014 `LanApprovalDecision`, which only the owning machine's own GUI
    /// may answer (contracts/lan-protocol.md).
    is_remote: bool,
}

/// The borrowed per-connection state threaded through the establish + message
/// read paths. Bundled into one `Copy` handle so both stay under Clippy's
/// argument threshold (mirrors [`StartSessionIds`]).
#[derive(Clone, Copy)]
struct ConnState<'a> {
    server: &'a IpcServerState,
    writer: &'a SharedWriter,
    attached_ids: &'a AttachedSessionIds,
}

struct CreateSessionRequest {
    workspace_id: WorkspaceId,
    split_direction: Option<scribe_common::protocol::LayoutDirection>,
    cwd: Option<std::path::PathBuf>,
    size: Option<TerminalSize>,
    command: Option<Vec<String>>,
    env_envelope_id: Option<String>,
}

#[derive(Clone, Copy)]
struct SessionRuntimeContext<'a> {
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
    live_sessions: &'a LiveSessionRegistry,
    /// Spec 010 C7: per-window clipboard-gating capability map shared with
    /// the IPC server.
    window_clipboard_gating: &'a WindowClipboardGating,
}

#[derive(Clone, Copy)]
struct InitialAttachment<'a> {
    writer: Option<&'a SharedWriter>,
    attached_ids: Option<&'a AttachedSessionIds>,
}

/// Bundle of session/window identifiers passed to [`start_session`]. Grouped
/// into one struct so the argument count stays under Clippy's
/// `too_many_arguments` threshold.
#[derive(Clone, Copy)]
struct StartSessionIds {
    session: SessionId,
    workspace: WorkspaceId,
    window: WindowId,
}

/// State needed by the PTY reader task, extracted from `ManagedSession`.
struct PtyReaderState {
    session_id: SessionId,
    /// Window this session belongs to. Used by the OSC 52 gating arms to
    /// look up the attached client's `clipboard_gating` capability bit
    /// (spec 010 C7) without re-reading the workspace manager on every
    /// event.
    window_id: WindowId,
    child_pid: u32,
    pty_read: ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    /// Per-window clipboard-gating capability map shared with the IPC
    /// server (spec 010 C7).
    window_clipboard_gating: WindowClipboardGating,
    /// Per-session OSC 52 gating state (spec 010 E3). Wave 2 only uses
    /// `outstanding_prompt` + `policy`; the burst-reuse fields land later.
    //
    // @lat: [[server#Sessions#Clipboard Gating]]
    clipboard_burst: crate::clipboard_state::ClipboardBurstState,
    /// Pending OSC 52 read requests awaiting the client's
    /// `ClipboardBridgeReadReply`, keyed by the `PromptId` echoed in the
    /// bridge request. Each entry holds the formatter alacritty handed us
    /// so we can rebuild the OSC 52 reply once the host clipboard text
    /// arrives.
    pending_clipboard_reads: HashMap<scribe_common::protocol::PromptId, ClipboardReplyFormatter>,
    /// Most-recently emitted prompt that has yet to be acknowledged.
    /// Stores both the deferred op so the response handler can replay the
    /// PTY-side reply (writes: forward bridge; reads: forward bridge or
    /// reply empty) using the same data the prompt arm captured.
    pending_clipboard_prompt: Option<PendingClipboardPrompt>,
    /// Inbound channel for client→PTY-reader OSC 52 control messages
    /// (`ClipboardPromptResponse`, `ClipboardBridgeReadReply`). The matching
    /// sender hangs off [`LiveSession::clipboard_command_tx`] and is the
    /// only path back into the reader task from the client message
    /// dispatcher.
    clipboard_command_rx: tokio::sync::mpsc::UnboundedReceiver<ClipboardCommand>,
    /// Reusable buffer for OSC events — cleared between iterations to avoid
    /// allocating a new `Vec` on every PTY read.
    osc_events: Vec<MetadataEvent>,
    /// Last known CWD from `/proc/pid/cwd`, used to detect changes triggered
    /// by title-change events (for shells that emit OSC 0 but not OSC 7).
    last_proc_cwd: Option<std::path::PathBuf>,
    /// Strips ED 3 (`\x1b[3J`) from supported AI sessions to preserve scrollback.
    ed3_filter: Ed3Filter,
    /// Neutralises Claude Code's picker-truncation 3rd-redraw to keep typed
    /// input visible (workaround for an `alacritty_terminal` row-tracking
    /// off-by-one vs xterm at the wrap-pending boundary).
    claude_picker_filter: ClaudePickerTruncationFilter,
    /// Upgrades bare LF to CRLF in PTY output so `alacritty_terminal`'s
    /// `linefeed` clears `input_needs_wrap` (via the inserted CR) — works
    /// around an upstream bug where wrap+LF advances the cursor by 2 rows
    /// instead of 1, breaking cursor-up redraws like release.sh's bash
    /// progress panel. Always-on, no AI-provider gating.
    lf_crlf_filter: LfCrlfFilter,
    /// Last AI provider seen for this session, if any.
    ai_provider: Option<AiProvider>,
    /// Latest known terminal cell size in pixels for winsize replies.
    cell_width: u16,
    cell_height: u16,
    /// When `true`, suppress `CSI 3 J` in AI sessions to preserve scrollback.
    preserve_ai_scrollback: Arc<AtomicBool>,
    /// Shared scrollback limit for trimming duplicate AI redraw history.
    scrollback_lines: Arc<AtomicUsize>,
    /// Duplicate-redraw trim baseline for the current AI scrollback epoch.
    preserved_ai_scrollback: PreservedAiScrollback,
    /// Waiting for the first filtered redraw in the epoch to commit.
    pending_ai_scrollback_baseline: bool,
}

/// A running session in the server-wide registry. Lives independently of
/// any client connection — the `client_writer` is set/cleared as clients
/// attach and detach.
pub struct LiveSession {
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    resize_fd: Arc<OwnedFd>,
    pub(crate) term:
        Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    child_pid: u32,
    /// `pub(crate)` so `hook_ingress` can clone the writer when routing
    /// inbound `HookEvent`s through `send_metadata_event`.
    pub(crate) client_writer: ClientWriter,
    attachment: SessionAttachment,
    workspace_id: WorkspaceId,
    shell_name: String,
    /// Last-known terminal title (OSC 0/2), persisted for reconnect.
    title: String,
    /// Last-known provider task label, persisted separately from OSC 0/2 titles.
    task_label: Option<String>,
    /// Last-known working directory (OSC 7), persisted for reconnect.
    cwd: Option<std::path::PathBuf>,
    /// Last-known remote/tmux context reported by shell integration.
    context: Option<scribe_common::protocol::SessionContext>,
    /// Last-known AI process state (OSC 1337), persisted for reconnect.
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
    /// Launch-time AI provider hint used when the session CLI does not emit
    /// explicit provider metadata.
    ai_provider_hint: Option<AiProvider>,
    /// Latest known terminal cell size in pixels.
    cell_width: u16,
    cell_height: u16,
    /// Keep the Pty alive so the child process isn't killed by SIGHUP on Drop.
    /// `None` for sessions restored from a hot-reload handoff. Taken and leaked
    /// by `defuse_for_handoff` during hot-reload to prevent SIGHUP.
    pty: Option<alacritty_terminal::tty::Pty>,
    /// Screen snapshot from a hot-reload handoff, sent to the first client
    /// that attaches. Taken (cleared) after first use.
    pub(crate) handoff_snapshot: Option<scribe_common::screen::ScreenSnapshot>,
    /// Shared runtime flag updated by config reloads.
    preserve_ai_scrollback: Arc<AtomicBool>,
    /// Shared runtime scrollback limit updated by config reloads.
    scrollback_lines: Arc<AtomicUsize>,
    /// The window that requested this session at create time. Stashed on
    /// the session itself (rather than re-derived from the workspace
    /// manager) so the clean-close path in [`handle_close_session`] can
    /// route the env-envelope delete after the session→window mapping has
    /// been torn down. Stable for the session's lifetime.
    ///
    /// `pub(crate)` so [`crate::hook_ingress`] can read it when routing an
    /// `EnvChanged` event into [`crate::env_store::EnvStoreState::schedule_persist`].
    pub(crate) env_window_id: WindowId,
    /// Launch-record id (== env-envelope id) naming this session's
    /// `<state_dir>/restore/env/<window_id>/<launch_id>.envz` file plus
    /// its keystore DEK. `Some` only for cold-restart replays that
    /// re-issued a `LaunchRecord` via `CreateSession.env_envelope_id`;
    /// `None` for fresh first-time creations and for handoff-restored
    /// sessions (handoff keeps env on the existing PTY across reload).
    ///
    /// `pub(crate)` so [`crate::hook_ingress`] can read it when routing an
    /// `EnvChanged` event into [`crate::env_store::EnvStoreState::schedule_persist`].
    pub(crate) env_envelope_id: Option<String>,
    /// Sender into the PTY reader task's OSC 52 control channel (spec 010
    /// C4). The client message dispatcher forwards
    /// `ClipboardPromptResponse` and `ClipboardBridgeReadReply` here so
    /// they reach `handle_clipboard_prompt_response` /
    /// `handle_clipboard_bridge_read_reply` on the owning reader task.
    pub(crate) clipboard_command_tx: tokio::sync::mpsc::UnboundedSender<ClipboardCommand>,
}

pub struct AttachSessionData {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub shell_name: String,
    pub client_writer: ClientWriter,
    pub attachment: SessionAttachment,
    pub term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    pub resize_fd: Arc<OwnedFd>,
    pub target_dims: Option<TerminalSize>,
    pub has_handoff_snapshot: bool,
}

impl LiveSession {
    pub fn prepare_attach_data(
        &mut self,
        session_id: SessionId,
        target_dims: Option<TerminalSize>,
    ) -> AttachSessionData {
        if let Some(size) = target_dims.filter(|size| size.has_pixels()) {
            self.cell_width = size.cell_width;
            self.cell_height = size.cell_height;
        }

        AttachSessionData {
            session_id,
            workspace_id: self.workspace_id,
            shell_name: self.shell_name.clone(),
            client_writer: Arc::clone(&self.client_writer),
            attachment: Arc::clone(&self.attachment),
            term: Arc::clone(&self.term),
            resize_fd: Arc::clone(&self.resize_fd),
            target_dims,
            has_handoff_snapshot: self.handoff_snapshot.is_some(),
        }
    }

    pub fn take_handoff_snapshot(&mut self) -> Option<ScreenSnapshot> {
        self.handoff_snapshot.take()
    }
}

/// Closure shape produced by `alacritty_terminal` for OSC 52 read replies.
/// Defined in [`crate::clipboard_state`] so [`crate::clipboard_state::DeferredRequest`]
/// can hold a parked formatter while a burst-deferred read waits on a prompt.
use crate::clipboard_state::ClipboardReplyFormatter;

/// Client→PTY-reader control message for OSC 52 prompt resolution and host
/// clipboard bridge replies. The client dispatch task pushes one of these
/// onto each session's clipboard channel; the reader task drains them on
/// every metadata pass.
pub enum ClipboardCommand {
    PromptResponse {
        request_id: scribe_common::protocol::PromptId,
        decision: scribe_common::protocol::ClipboardDecision,
    },
    BridgeReadReply {
        request_id: scribe_common::protocol::PromptId,
        payload: Result<String, scribe_common::protocol::BridgeError>,
    },
    /// Spec 010 T036: hot-reload the per-session policy snapshot when the
    /// `ConfigReloaded` handler swaps `terminal.clipboard.*` keys on disk.
    /// The PTY reader task replaces `ClipboardBurstState.policy` in place so
    /// the next OSC 52 op sees the new mode without waiting for a server
    /// restart (FR-010).
    RefreshPolicy { policy: scribe_common::config::ClipboardPolicyConfig },
}

/// State captured when the server emits a `ClipboardPromptRequest` and is
/// waiting on the user's resolution. On `Allow` the server replays this
/// against either the bridge (writes; reads stash a fresh read formatter
/// keyed by `request_id`) or, for headless edges, the deferred PTY-side
/// empty reply.
struct PendingClipboardPrompt {
    request_id: scribe_common::protocol::PromptId,
    op: scribe_common::protocol::ClipboardOp,
    selection: scribe_common::protocol::ClipboardSelection,
    /// `Some` for `op == Write`: the payload accepted at the size-cap check,
    /// ready to forward to the bridge on Allow.
    write_payload: Option<String>,
    /// `Some` for `op == Read`: the alacritty formatter parked for the
    /// deferred OSC 52 reply (empty on Deny, host clipboard on Allow).
    read_formatter: Option<ClipboardReplyFormatter>,
}

/// Process-wide monotonic `PromptId` allocator (spec 010 contract C3).
/// One counter for the whole server is sufficient because `PromptId` only
/// needs to be unique while a prompt is in flight; collisions across
/// long-running sessions are not observable.
fn allocate_prompt_id() -> scribe_common::protocol::PromptId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    scribe_common::protocol::PromptId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Map an alacritty `ClipboardType` to its protocol-level
/// `ClipboardSelection` enum (spec 010 E2).
fn clipboard_selection_from(
    kind: alacritty_terminal::term::ClipboardType,
) -> scribe_common::protocol::ClipboardSelection {
    match kind {
        alacritty_terminal::term::ClipboardType::Clipboard => {
            scribe_common::protocol::ClipboardSelection::Clipboard
        }
        alacritty_terminal::term::ClipboardType::Selection => {
            scribe_common::protocol::ClipboardSelection::Primary
        }
    }
}

/// Build the head-and-tail truncated preview required by FR-006 for OSC 52
/// write confirmation prompts. Mirrors the disallowed-scheme dialog's body
/// truncation rule so the user sees the start and end of the payload.
fn clipboard_write_preview(text: &str) -> String {
    const MAX: usize = 96;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX {
        return text.to_owned();
    }
    let budget = MAX.saturating_sub(3);
    let head = budget.div_ceil(2);
    let tail = budget - head;
    let mut out: String = chars.iter().take(head).collect();
    out.push_str("...");
    out.extend(chars.iter().skip(chars.len() - tail));
    out
}

async fn attached_contains(attached_ids: &AttachedSessionIds, session_id: SessionId) -> bool {
    attached_ids.lock().await.contains(&session_id)
}

async fn attached_insert(attached_ids: &AttachedSessionIds, session_id: SessionId) {
    attached_ids.lock().await.insert(session_id);
}

async fn attached_extend(
    attached_ids: &AttachedSessionIds,
    ids: impl IntoIterator<Item = SessionId>,
) {
    attached_ids.lock().await.extend(ids);
}

async fn attached_remove(attached_ids: &AttachedSessionIds, session_id: SessionId) {
    attached_ids.lock().await.remove(&session_id);
}

async fn attached_snapshot(attached_ids: &AttachedSessionIds) -> HashSet<SessionId> {
    attached_ids.lock().await.clone()
}

async fn clear_session_attachment(attachment: &SessionAttachment) {
    *attachment.lock().await = None;
}

async fn remove_from_session_attachment(attachment: &SessionAttachment, session_id: SessionId) {
    let attached_ids = attachment.lock().await.clone();
    if let Some(attached_ids) = attached_ids {
        attached_remove(&attached_ids, session_id).await;
    }
}

/// Start the IPC accept loop on an already-bound listener.
pub async fn start_ipc_server(
    listener: UnixListener,
    server: IpcServerState,
) -> Result<(), ScribeError> {
    let connection_limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                if !verify_peer_uid(&stream) {
                    continue;
                }

                let Ok(permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                    warn!("connection limit ({MAX_CONNECTIONS}) reached, rejecting client");
                    continue;
                };

                info!("client connected");
                let server = server.clone();
                tokio::spawn(async move {
                    handle_client(stream, server).await;
                    drop(permit);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

// ── Feature 013: remote-control TCP transport ───────────────────────────────

/// Maximum simultaneous REMOTE (TCP) connections, separate from the 32 local
/// (`MAX_CONNECTIONS`). Excess connections are refused with a typed `Busy` after
/// the preamble is read (contracts/remote-protocol.md Transport).
const REMOTE_CONNECTION_CAP: usize = 8;

/// Admission cap on pending (pre-authorization) remote connections, acquired the
/// instant a TCP stream is accepted — before the handler is spawned — so a flood
/// of half-open dialers cannot spawn unbounded handshake tasks or allocate
/// pre-auth (mirrors [`start_ipc_server`]'s pre-spawn `try_acquire_owned`). Kept
/// generously above [`REMOTE_CONNECTION_CAP`] so in-flight handshakes never
/// starve the authorized-connection slots; excess dialers are dropped
/// immediately.
const REMOTE_PENDING_HANDSHAKE_CAP: usize = 64;

/// Feature 014: maximum simultaneous LAN (mutual-TLS) connections — the LAN
/// transport's OWN cap, wholly independent of the tailnet
/// [`REMOTE_CONNECTION_CAP`] so LAN load can never consume tailnet admission
/// slots (analysis C4/S1). Mirrors the tailnet budget; excess LAN connections
/// are refused with `Busy` (feature 014 T011).
const LAN_CONNECTION_CAP: usize = 8;

/// Feature 014: admission cap on pending (pre-authorization) LAN connections —
/// the LAN counterpart to [`REMOTE_PENDING_HANDSHAKE_CAP`], acquired the instant
/// a LAN TCP stream is accepted so a flood of half-open TLS dialers cannot spawn
/// unbounded handshake work. Independent of the tailnet cap so neither transport
/// can starve the other.
const LAN_PENDING_HANDSHAKE_CAP: usize = 64;

/// Hard size cap on the pre-auth `RemoteHandshake` preamble frame. The preamble
/// carries only a version number plus two short strings, so a few KiB is ample;
/// bounding it far below the shared 64 MiB frame budget denies an unauthenticated
/// dialer a large heap allocation from a forged length prefix.
const REMOTE_PREAMBLE_MAX_BYTES: u32 = 8 * 1024;

/// Per-remote-connection cap on queued droppable `PtyOutput` payload bytes
/// (feature 013 T029, research D5, PR-004). When the backlog would exceed this
/// the whole `PtyOutput` queue is shed and its sessions are marked replay-dirty
/// for a fresh full replay, so a stalled consumer's memory stays bounded without
/// ever back-pressuring the PTY. Control/replay frames are never counted here.
const REMOTE_OUTPUT_QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// Per-remote-connection cap on TOTAL queued bytes across every frame kind —
/// including the non-droppable `Keep` lane (`SessionReplay`, `SessionCreated`,
/// `TitleChanged`, `ClipboardBridgeWrite`, …). Prevents an unbounded `Keep`
/// backlog on a stalled link: on breach the droppable `PtyOutput` backlog is shed
/// first, and if the queue is STILL over ceiling (a pure control-frame flood the
/// link cannot drain) the connection is closed so its memory use stays bounded
/// (FR-013). Sits above [`REMOTE_OUTPUT_QUEUE_BYTES`] to leave headroom for a
/// legitimate multi-session initial attach replay.
const REMOTE_OUTPUT_QUEUE_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Per-remote-connection cap on the number of queued frames, bounding a flood of
/// small `Keep` control frames the byte ceiling would under-count. Same
/// shed-then-close policy as [`REMOTE_OUTPUT_QUEUE_TOTAL_BYTES`].
const REMOTE_OUTPUT_QUEUE_MAX_FRAMES: usize = 8192;

/// Nominal byte cost charged to a queued control frame whose exact serialized
/// size is not worth computing. The two high-volume streams are sized precisely
/// (`PtyOutput` by payload length, `SessionReplay` by its compressed blob); every
/// other frame is small, so a flat nominal keeps the total-byte accounting cheap
/// while the frame-count ceiling backstops tiny-frame floods.
const REMOTE_FRAME_NOMINAL_BYTES: usize = 256;

/// Upper bound on how long a freshly accepted remote connection may take to send
/// its `RemoteHandshake` preamble before it is dropped, so a silent peer cannot
/// hold an accept slot open indefinitely.
const REMOTE_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Idle-read timeout for an ESTABLISHED remote connection: if no frame arrives
/// within this window the connection is treated as dead and its scarce
/// authorized-connection slot ([`REMOTE_CONNECTION_CAP`]) is reclaimed, so a peer
/// that vanished without a FIN/RST (laptop sleep, Wi-Fi roam) cannot leak a slot
/// indefinitely. Deliberately generous — remote input is bursty and a peer that
/// is only watching output sends nothing — so an idle-but-live viewer is rarely
/// tripped; when it is, sessions keep running and the client auto-reconnects and
/// reconverges (FR-011). Applies to remote (TCP) connections ONLY; local
/// Unix-socket connections keep today's untimed reads. This is the application
/// backstop for a peer that is TCP-alive but app-silent; a peer whose TCP path
/// has vanished (no FIN/RST) is reclaimed faster by tuned keepalive (see
/// [`enable_tcp_keepalive`]).
const REMOTE_IDLE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Idle time before the OS starts sending TCP keepalive probes on an accepted
/// remote connection, and the interval between probes. With the OS-default probe
/// count this drops a vanished (FIN/RST-less) peer — and frees its authorized
/// slot — in a few minutes instead of waiting out [`REMOTE_IDLE_READ_TIMEOUT`],
/// with no false positives on a live-but-idle viewer (a live TCP stack ACKs the
/// probes even when the app sends nothing).
const REMOTE_KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_secs(60);
const REMOTE_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Upper bound on the best-effort `ServerMessage::RemoteDisconnect` write when a
/// remote connection is severed on disable (T023). Each severed connection sends
/// concurrently on its own task, so this also bounds how long the whole sever
/// takes — comfortably inside the 2-second disable budget (FR-016).
const REMOTE_SEVER_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// tracing target for the feature-013 remote audit log lines
/// (contracts/settings-and-config.md Audit log surface).
const REMOTE_AUDIT_TARGET: &str = "scribe_server::remote_audit";

/// How an accepted remote connection is named in the audit log. Feature 013's
/// tailnet path logs `remote: … peer=<node> user=<login>` (kept byte-identical);
/// feature 014's LAN path logs `lan: … device=<label> id=<short>` per
/// contracts/settings-and-config.md, so the accepted/disconnect audit lines match
/// the transport a connection actually arrived on.
enum RemoteAudit {
    /// Feature 013 tailnet peer — `remote:` audit lines.
    Tailnet,
    /// Feature 014 LAN device — `lan:` audit lines, tagged with the pinned
    /// device's short id.
    Lan { device_id_short: String },
}

/// Why the LAN transport is being taken dormant, for the `lan: dormant …` audit
/// line (contracts/settings-and-config.md Audit log surface). Only the two
/// user-meaningful transitions are audited; the fail-closed operational cases
/// (enabled + trusted but no bindable physical-LAN address, or an unavailable
/// keyring/identity) are logged at warn level by
/// [`apply_lan`](RemoteControl::apply_lan) and pass `None`, emitting no line.
#[derive(Clone, Copy)]
enum LanDormantReason {
    /// `remote.lan.enabled` is off.
    Disabled,
    /// LAN access is on but the current network is not trusted (FR-018).
    NetworkUntrusted,
}

/// The identity of an accepted remote connection, carried into the shared
/// dispatch so the accepted/disconnect audit lines can name the peer. For a
/// tailnet peer `node_name`/`login_name` are the tailnet node + account; for a
/// LAN device `node_name` is the peer's advertised label and `login_name` is
/// empty (LAN carries no account). [`audit`](Self::audit) selects the audit-line
/// format.
struct RemoteContext {
    node_name: String,
    login_name: String,
    /// Selects the transport-specific audit-line format (feature 014).
    audit: RemoteAudit,
    /// Feature 013 (T023): fires when remote access is disabled. The shared read
    /// paths `select!` on it so a disable drops this connection out of its loop
    /// and through the normal detach cleanup (owning sessions untouched) before
    /// the socket closes.
    sever: tokio::sync::oneshot::Receiver<()>,
}

/// The display identity of an accepted remote connection, retained after the
/// sever signal is split off [`RemoteContext`] so the accepted/disconnect audit
/// lines can still name the peer.
struct RemoteIdentity {
    node_name: String,
    login_name: String,
    /// Selects the transport-specific audit-line format (feature 014).
    audit: RemoteAudit,
}

// ── Feature 013 (T029): bounded per-remote-connection output queue ───────────
//
// A slow tailnet link must never block the fan-out hot path (research D5,
// FR-013). If `send_pty_output` / `send_to_client` wrote straight to a stalled
// TCP socket, the owning session's `pty_reader_task` would wedge on the write,
// back-pressuring the PTY and freezing the running program — and, because the
// authoritative `Term` is shared, stalling every other client with it. So each
// REMOTE connection interposes this bounded queue: its `SharedWriter` only
// enqueues (never awaits the socket), and a single drain task owns the write
// half. When queued `PtyOutput` would exceed the cap the whole backlog is
// dropped and its sessions are marked replay-dirty; the drain task then sends
// each a fresh full `SessionReplay` once the link drains (catch-up-to-current,
// the tmux `%pause`→`capture-pane` model). Local Unix-socket connections never
// build one — they keep writing inline exactly as before (see `ClientSink` and
// `handle_client`).

/// Enqueue handle into a remote connection's bounded output queue. Cloneable and
/// cheap: every `SharedWriter` clone for the connection (the window-registry slot
/// and each attached session's `client_writer`) holds one, all funneling into the
/// single queue drained by [`remote_output_drain`].
#[derive(Clone)]
pub struct RemoteSink(Arc<RemoteOutputShared>);

/// Shared state between a remote connection's [`RemoteSink`] producers and its
/// one drain task. The queue is guarded by a *std* mutex: every critical section
/// is a few non-blocking `VecDeque` / `HashSet` operations and never spans an
/// `.await`, so producers never park on the link (and the `!Send` guard makes the
/// compiler enforce that discipline in the spawned drain task).
struct RemoteOutputShared {
    inner: std::sync::Mutex<RemoteQueueInner>,
    /// Wakes the drain task when frames are enqueued, a session goes replay-dirty,
    /// or the connection is shutting down.
    notify: tokio::sync::Notify,
}

struct RemoteQueueInner {
    /// Frames awaiting the link, in send order.
    frames: VecDeque<OutFrame>,
    /// Running total of queued droppable `PtyOutput` payload bytes — the quantity
    /// the overflow cap ([`REMOTE_OUTPUT_QUEUE_BYTES`]) governs.
    queued_pty_bytes: usize,
    /// Running total of queued bytes across EVERY frame kind (droppable and
    /// `Keep`), governed by [`REMOTE_OUTPUT_QUEUE_TOTAL_BYTES`] so an unbounded
    /// `Keep` backlog on a stalled link is bounded too, not just `PtyOutput`.
    queued_total_bytes: usize,
    /// Sessions whose `PtyOutput` backlog was dropped and that therefore owe a
    /// fresh full replay. While a session sits here its live `PtyOutput` is
    /// suppressed (the pending replay supersedes it), which is what lets the queue
    /// actually drain on a saturated link so the replay can be sent.
    dirty: HashSet<SessionId>,
    /// Set at teardown so the drain task flushes what remains and exits.
    closed: bool,
}

/// One queued frame. Only [`OutFrame::Pty`] is droppable by the overflow policy;
/// every other message (takeover notice, session-exit, workspace update, the
/// initial attach replay, …) is kept so it is never silently lost. Each variant
/// carries its accounted byte size so both the `PtyOutput` cap and the total-queue
/// cap can be maintained in O(1) on pop.
enum OutFrame {
    Pty { session_id: SessionId, bytes: usize, msg: ServerMessage },
    Keep { bytes: usize, msg: ServerMessage },
}

impl RemoteQueueInner {
    /// Pop the next queued frame's message in send order, decrementing both the
    /// `PtyOutput` byte total and the whole-queue byte total so the overflow caps
    /// track what is still buffered.
    fn pop_message(&mut self) -> Option<ServerMessage> {
        match self.frames.pop_front()? {
            OutFrame::Pty { bytes, msg, .. } => {
                self.queued_pty_bytes = self.queued_pty_bytes.saturating_sub(bytes);
                self.queued_total_bytes = self.queued_total_bytes.saturating_sub(bytes);
                Some(msg)
            }
            OutFrame::Keep { bytes, msg } => {
                self.queued_total_bytes = self.queued_total_bytes.saturating_sub(bytes);
                Some(msg)
            }
        }
    }
}

impl RemoteOutputShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, RemoteQueueInner> {
        // The only lock holders are the enqueue path and the drain task, both
        // doing panic-free `VecDeque` / `HashSet` work, so poisoning cannot occur
        // in practice; recover rather than propagate if it somehow does.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl RemoteSink {
    /// Enqueue one `ServerMessage` for the drain task. Never blocks on the link.
    ///
    /// `PtyOutput` is the sole droppable, high-volume stream: while its session is
    /// replay-dirty it is suppressed, and when the queued `PtyOutput` backlog would
    /// exceed [`REMOTE_OUTPUT_QUEUE_BYTES`] the entire backlog is dropped and every
    /// affected session (this one included) is marked replay-dirty. Every other
    /// message is kept in order, but the whole queue is bounded too
    /// ([`REMOTE_OUTPUT_QUEUE_TOTAL_BYTES`] / [`REMOTE_OUTPUT_QUEUE_MAX_FRAMES`]):
    /// on breach the droppable backlog is shed, and a queue still over ceiling is
    /// a hopelessly stalled link, so the connection is closed (bounded resource
    /// use, FR-013).
    ///
    /// The (potentially multi-MB) message clone is built BEFORE the queue lock is
    /// taken, so the std mutex is only ever held for O(1) `VecDeque`/`HashSet`
    /// bookkeeping — never for a large `memcpy`.
    fn enqueue(&self, msg: &ServerMessage) {
        let bytes = out_frame_bytes(msg);
        let pty_session = match msg {
            ServerMessage::PtyOutput { session_id, .. } => Some(*session_id),
            _ => None,
        };
        // Clone off the lock (see doc-comment): the frame is prepared here and
        // simply discarded if the overflow policy below decides to drop it.
        let frame = pty_session.map_or_else(
            || OutFrame::Keep { bytes, msg: msg.clone() },
            |session_id| OutFrame::Pty { session_id, bytes, msg: msg.clone() },
        );

        let mut g = self.0.lock();
        if g.closed {
            return;
        }
        if let Some(session_id) = pty_session {
            if g.dirty.contains(&session_id) {
                // A fresh replay is already pending for this session; its live
                // output is superseded — drop it and skip the wakeup.
                return;
            }
            if g.queued_pty_bytes.saturating_add(bytes) > REMOTE_OUTPUT_QUEUE_BYTES {
                // Overflow: shed the whole `PtyOutput` backlog and let a fresh
                // replay catch every affected session (this one too) back up.
                drop_pty_backlog(&mut g);
                g.dirty.insert(session_id);
            } else {
                g.queued_pty_bytes += bytes;
                g.queued_total_bytes += bytes;
                g.frames.push_back(frame);
            }
        } else {
            g.queued_total_bytes += bytes;
            g.frames.push_back(frame);
        }
        enforce_queue_ceiling(&mut g);
        drop(g);
        self.0.notify.notify_one();
    }

    /// Mark the queue closed and wake the drain task so it flushes what remains
    /// (e.g. a final `RemoteDisconnect`) and exits, dropping the write half and
    /// closing the socket. Idempotent.
    fn shutdown(&self) {
        self.0.lock().closed = true;
        self.0.notify.notify_one();
    }
}

/// Drop every queued `PtyOutput` frame, marking each dropped session replay-dirty
/// so the drain task sends it a fresh full replay once the link drains. Kept
/// (control / replay) frames retain their relative order.
fn drop_pty_backlog(g: &mut RemoteQueueInner) {
    let mut kept = VecDeque::with_capacity(g.frames.len());
    for frame in std::mem::take(&mut g.frames) {
        match frame {
            OutFrame::Pty { session_id, .. } => {
                g.dirty.insert(session_id);
            }
            keep @ OutFrame::Keep { .. } => kept.push_back(keep),
        }
    }
    g.frames = kept;
    // Only the droppable `PtyOutput` bytes leave the queue; the `Keep` bytes that
    // remain still count against the total.
    g.queued_total_bytes = g.queued_total_bytes.saturating_sub(g.queued_pty_bytes);
    g.queued_pty_bytes = 0;
}

/// Byte cost charged to a queued frame. The two high-volume streams are sized
/// precisely so the total-queue cap tracks real memory; every other (small)
/// control frame is charged a flat [`REMOTE_FRAME_NOMINAL_BYTES`], with the
/// frame-count ceiling backstopping tiny-frame floods.
fn out_frame_bytes(msg: &ServerMessage) -> usize {
    match msg {
        ServerMessage::PtyOutput { data, .. } => data.len(),
        ServerMessage::SessionReplay { replay, .. } => replay.replay_zstd.len(),
        _ => REMOTE_FRAME_NOMINAL_BYTES,
    }
}

/// Bound total queue memory after an enqueue (feature 013 flow-control hardening,
/// FR-013). When the queue exceeds its total-byte or frame-count ceiling — a
/// stalled link accumulating `Keep` frames the `PtyOutput` cap does not govern —
/// shed the droppable backlog first; if it is STILL over ceiling the link cannot
/// keep up even with output dropped, so mark the connection closed for teardown
/// (the client auto-reconnects to a fresh replay). A no-op on a healthy queue.
fn enforce_queue_ceiling(g: &mut RemoteQueueInner) {
    if g.queued_total_bytes <= REMOTE_OUTPUT_QUEUE_TOTAL_BYTES
        && g.frames.len() <= REMOTE_OUTPUT_QUEUE_MAX_FRAMES
    {
        return;
    }
    drop_pty_backlog(g);
    if g.queued_total_bytes > REMOTE_OUTPUT_QUEUE_TOTAL_BYTES
        || g.frames.len() > REMOTE_OUTPUT_QUEUE_MAX_FRAMES
    {
        warn!(
            queued_bytes = g.queued_total_bytes,
            frames = g.frames.len(),
            "remote output queue over ceiling after shedding backlog; closing stalled connection"
        );
        g.closed = true;
    }
}

/// Build a remote connection's output queue and spawn its drain task (which owns
/// the TCP write half). Returns the enqueue handle to install into the
/// connection's `SharedWriter` plus the drain task's join handle, awaited
/// (bounded) at teardown by [`shutdown_remote_output`].
fn spawn_remote_output<W>(
    write_half: W,
    server: IpcServerState,
) -> (RemoteSink, tokio::task::JoinHandle<()>)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let shared = Arc::new(RemoteOutputShared {
        inner: std::sync::Mutex::new(RemoteQueueInner {
            frames: VecDeque::new(),
            queued_pty_bytes: 0,
            queued_total_bytes: 0,
            dirty: HashSet::new(),
            closed: false,
        }),
        notify: tokio::sync::Notify::new(),
    });
    let drain = tokio::spawn(remote_output_drain(Arc::clone(&shared), write_half, server));
    (RemoteSink(shared), drain)
}

/// The single writer for a remote connection: flush queued frames to the socket,
/// then send a fresh full `SessionReplay` for any replay-dirty session, then park
/// until more work arrives (or the connection closes). This is the ONLY task that
/// writes the remote socket, so frame order on the wire is exactly enqueue order.
async fn remote_output_drain<W>(
    shared: Arc<RemoteOutputShared>,
    mut write_half: W,
    server: IpcServerState,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        // 1. Flush every queued frame, in order.
        loop {
            let Some(msg) = shared.lock().pop_message() else { break };
            if write_message(&mut write_half, &msg).await.is_err() {
                shared.lock().closed = true;
                return;
            }
        }

        // 2. Catch-up: send a fresh full replay for each replay-dirty session.
        let dirty: Vec<SessionId> = {
            let g = shared.lock();
            g.dirty.iter().copied().collect()
        };
        for session_id in dirty {
            if !send_resync_replay(&shared, &mut write_half, &server, session_id).await {
                return;
            }
        }

        // 3. Park until there is more to do, unless we are draining to close. Work
        //    that arrived during step 2 keeps us looping instead of sleeping.
        {
            let g = shared.lock();
            if !g.frames.is_empty() || !g.dirty.is_empty() {
                continue;
            }
            if g.closed {
                break;
            }
        }
        shared.notify.notified().await;
    }
    // `write_half` drops here → the socket's write side closes.
}

/// Build and send one replay-dirty session's catch-up replay, reusing the same
/// `take_session_replay` primitive as reattach. The dirty flag clears right after
/// the snapshot but before the (possibly slow) socket write, so live output
/// resumes queuing immediately and lands *after* this full-state replay. Returns
/// `false` only when the socket write failed (the drain task then stops).
async fn send_resync_replay<W>(
    shared: &Arc<RemoteOutputShared>,
    write_half: &mut W,
    server: &IpcServerState,
    session_id: SessionId,
) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let term = {
        let sessions = server.live_sessions.read().await;
        sessions.get(&session_id).map(|s| Arc::clone(&s.term))
    };
    let Some(term) = term else {
        // Session ended while dirty; nothing to replay.
        shared.lock().dirty.remove(&session_id);
        return true;
    };
    let replay =
        match crate::attach_flow::take_session_replay(session_id, &term, &server.live_sessions)
            .await
        {
            Ok(replay) => replay,
            Err(e) => {
                warn!(%session_id, "remote resync replay build failed: {e}");
                shared.lock().dirty.remove(&session_id);
                return true;
            }
        };
    // Clear dirty after the snapshot, before the write: live `PtyOutput` resumes
    // queuing now and follows this replay (catch-up-to-current). The brief
    // snapshot→clear window drops output like the pre-013 reattach window, and this
    // full-state replay supersedes it anyway.
    shared.lock().dirty.remove(&session_id);
    let msg = ServerMessage::SessionReplay { session_id, replay };
    if write_message(write_half, &msg).await.is_err() {
        shared.lock().closed = true;
        return false;
    }
    true
}

/// Stop a remote connection's drain task at teardown, bounded so a wedged link
/// cannot exceed the disable budget (FR-016). Any already-queued final frame
/// (e.g. the sever `RemoteDisconnect`) flushes first; then the write half drops
/// and the socket closes.
async fn shutdown_remote_output(sink: &RemoteSink, mut drain_task: tokio::task::JoinHandle<()>) {
    sink.shutdown();
    if tokio::time::timeout(REMOTE_SEVER_NOTICE_TIMEOUT, &mut drain_task).await.is_err() {
        drain_task.abort();
    }
}

/// Which remote transport a listener/connection belongs to (feature 014,
/// analysis C4/S1). Feature 013's tailnet (Tailscale) path and feature 014's LAN
/// (mutual-TLS) path each own an independent [`TransportControl`] — their own
/// `enabled` flag, admission caps, and sever registry — so disabling or going
/// dormant on one transport severs only that transport's connections and LAN
/// load can never starve tailnet admission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// Feature 013 tailnet (Tailscale) transport — behavior byte-identical to
    /// the pre-014 single-transport `RemoteControl`.
    Tailnet,
    /// Feature 014 LAN (mutual-TLS) transport.
    Lan,
}

/// One live accepted connection's sever handle, plus — for the LAN transport —
/// the pinned device that owns it, so a per-device revoke can target just its
/// connections without touching any other (feature 014, FR-010).
struct SeverHandle {
    /// Fires this connection out of its message loop and through the normal
    /// detach cleanup (owning sessions untouched, FR-016/FR-010).
    sever: tokio::sync::oneshot::Sender<()>,
    /// The pinned LAN `device_id = SHA-256(SPKI)` this connection authenticated
    /// as, or `None` for a tailnet connection (which carries no Scribe device
    /// identity). Drives the LAN `device_id -> connection-id` index.
    device_id: Option<DeviceId>,
}

/// A single transport's registry of live accepted connections (feature 013 T023,
/// generalized per-transport in feature 014). Every connection registers its
/// sever channel under a process-unique id; firing a sever drops that connection
/// out of its loop through the normal detach path. For the LAN transport a
/// secondary `device_id -> {connection ids}` index is maintained so a per-device
/// revoke can later sever only that device's connections (FR-010); a tailnet
/// connection has no device identity, so its entries carry `None` and the index
/// stays empty. Both maps are mutated together under the enclosing `Mutex`, so
/// they cannot drift. A per-device revoke queries this index by `device_id` via
/// [`drain_device`](SeverRegistry::drain_device) (feature 014 T027).
#[derive(Default)]
struct SeverRegistry {
    /// connection id → its sever handle.
    by_conn: HashMap<u64, SeverHandle>,
    /// device id → the connection ids it currently owns (LAN only; empty for the
    /// tailnet transport).
    by_device: HashMap<DeviceId, HashSet<u64>>,
}

impl SeverRegistry {
    /// Register a connection's sever channel under `id`, additionally indexing it
    /// by `device_id` when the connection carries one (LAN).
    fn insert(
        &mut self,
        id: u64,
        sever: tokio::sync::oneshot::Sender<()>,
        device_id: Option<DeviceId>,
    ) {
        if let Some(device_id) = device_id {
            self.by_device.entry(device_id).or_default().insert(id);
        }
        self.by_conn.insert(id, SeverHandle { sever, device_id });
    }

    /// Drop a connection's registration once its task ends, keeping the device
    /// index consistent. A no-op if a sever already drained it.
    fn remove(&mut self, id: u64) {
        let Some(handle) = self.by_conn.remove(&id) else {
            return;
        };
        let Some(device_id) = handle.device_id else {
            return;
        };
        let Some(ids) = self.by_device.get_mut(&device_id) else {
            return;
        };
        ids.remove(&id);
        if ids.is_empty() {
            self.by_device.remove(&device_id);
        }
    }

    /// Take every connection's sever channel, clearing both maps.
    fn drain_all(&mut self) -> Vec<tokio::sync::oneshot::Sender<()>> {
        self.by_device.clear();
        std::mem::take(&mut self.by_conn).into_values().map(|handle| handle.sever).collect()
    }

    /// Take the sever channels for every live connection owned by `device_id`,
    /// removing them from BOTH maps so the registry stays consistent. This is the
    /// per-device revoke consumer of the `device_id -> {connection ids}` index
    /// (feature 014 T027): a revoked device's own connections are severed while
    /// every other connection — and the owning sessions — are untouched (FR-010).
    /// Empty when the device owns no live connection (already gone, or it never
    /// held one).
    fn drain_device(&mut self, device_id: &DeviceId) -> Vec<tokio::sync::oneshot::Sender<()>> {
        let Some(ids) = self.by_device.remove(device_id) else {
            return Vec::new();
        };
        ids.into_iter()
            .filter_map(|id| self.by_conn.remove(&id))
            .map(|handle| handle.sever)
            .collect()
    }
}

/// The per-transport admission + sever state hosted inside [`RemoteControl`]
/// (feature 014, analysis C4/S1). Each transport (tailnet, LAN) owns an
/// independent instance so its listener can be enabled/disabled and its
/// connections severed without disturbing the other, and neither transport's
/// caps can starve the other's admission. The listener lifecycle (bind + accept
/// tasks) is owned by the [`remote_supervisor`] task; this struct only carries
/// the cross-task state the accept and dispatch paths read.
struct TransportControl {
    /// Live "this transport is accepting" flag. Read by the accept path to answer
    /// `Disabled` when a connection races a live disable; written by the
    /// supervisor. `true` only while at least one listener is bound.
    enabled: AtomicBool,
    /// This transport's connection cap, held for the life of each accepted
    /// connection (tailnet: [`REMOTE_CONNECTION_CAP`]; LAN: [`LAN_CONNECTION_CAP`]).
    conn_limit: Arc<tokio::sync::Semaphore>,
    /// This transport's pending (pre-authorization) handshake cap, acquired the
    /// instant a stream is accepted — before the handler is spawned — so a flood
    /// of half-open dialers cannot spawn unbounded handshake tasks (tailnet:
    /// [`REMOTE_PENDING_HANDSHAKE_CAP`]; LAN: [`LAN_PENDING_HANDSHAKE_CAP`]).
    handshake_limit: Arc<tokio::sync::Semaphore>,
    /// This transport's live-connection sever registry (with the LAN device
    /// index). Populated only past the handshake; the rebind path leaves it alone.
    connections: tokio::sync::Mutex<SeverRegistry>,
}

impl TransportControl {
    /// Create a stopped transport with its own connection and pending-handshake
    /// caps and an empty sever registry.
    fn new(conn_cap: usize, handshake_cap: usize) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            conn_limit: Arc::new(tokio::sync::Semaphore::new(conn_cap)),
            handshake_limit: Arc::new(tokio::sync::Semaphore::new(handshake_cap)),
            connections: tokio::sync::Mutex::new(SeverRegistry::default()),
        }
    }
}

/// Shared control handle for the remote-control listeners. Lives in
/// [`IpcServerState`]. The listener lifecycle itself is owned by a single
/// [`remote_supervisor`] task (started once at server startup); this handle only
/// carries the cross-task state the accept and dispatch paths need:
/// [`request_reload`](Self::request_reload) pokes the supervisor, and each
/// transport's [`TransportControl`] answers the disable-race refusal and holds
/// its admission caps + sever registry. Keeping the apply loop off the dispatch
/// call graph is deliberate — it keeps a wedged tailscaled from stalling client
/// message loops and avoids an async auto-trait inference cycle (dispatch →
/// apply → accept → dispatch).
///
/// Feature 014 (analysis C4/S1) split the former single-transport state into
/// per-transport [`tailnet`](Self::tailnet) and [`lan`](Self::lan) instances so
/// disabling or going dormant on one transport severs only its own connections,
/// plus a `device_id -> connection-id` index (inside each transport's
/// [`SeverRegistry`]) so a per-device revoke and a per-transport disable target
/// only the right connections (FR-010/FR-012). The tailnet path is preserved
/// byte-for-byte.
pub struct RemoteControl {
    /// Poked (synchronously, no await) whenever the config changes so the
    /// supervisor re-reads and re-applies the `[remote]` / `[remote.lan]` tables.
    reload: tokio::sync::Notify,
    /// Monotonic allocator for sever-registry connection ids, shared across
    /// transports so every live connection has a process-unique id.
    next_conn_id: AtomicU64,
    /// Feature 013 tailnet transport (caps [`REMOTE_CONNECTION_CAP`] /
    /// [`REMOTE_PENDING_HANDSHAKE_CAP`]); behavior byte-identical to the pre-014
    /// single-transport `RemoteControl`.
    tailnet: TransportControl,
    /// Feature 014 LAN transport (its OWN caps [`LAN_CONNECTION_CAP`] /
    /// [`LAN_PENDING_HANDSHAKE_CAP`], sever registry, and device index),
    /// independent of tailnet. The LAN listener is wired by feature 014 T010/T011.
    lan: TransportControl,
    /// Feature 014 LAN concurrent-pending-approval registry (a separate cap +
    /// per-hold timeout, analysis S1): correlates a pushed `LanApprovalRequest`
    /// with the owning client's `LanApprovalDecision` and bounds how many/long
    /// unapproved dialers may hold. Shared between the LAN accept path (which
    /// begins + awaits holds) and the local-socket decision handler (which
    /// resolves them, [`dispatch_message`]).
    pending_approvals: Arc<PendingApprovals>,
    /// Feature 014 trusted-device pin store: the strict `device_id` pin check the
    /// TLS pinning verifier consults and the accept path writes on approval, plus
    /// the revoke/list surface (T027/T013). Shared behind a `std::sync::Mutex`;
    /// every critical section is short and off the `.await` path.
    trusted_devices: SharedTrustedDevices,
    /// Feature 014 T021 trusted-networks activation store: the add/remove/list
    /// surface behind the local-only `AddCurrentNetworkTrusted` /
    /// `RemoveTrustedNetwork` / `ListTrustedNetworks` handlers, AND the very same
    /// store the supervisor's [`apply_lan`](Self::apply_lan) activation gate and
    /// the network-change watcher read — held here so a live add/remove and the
    /// gate observe ONE store, and removing the current network can poke a reload
    /// that takes the LAN transport dormant (FR-018). Shared behind a
    /// `std::sync::Mutex`; the `netdev` read + store I/O runs on the blocking pool.
    trusted_networks: SharedTrustedNetworks,
    /// Feature 014 T013: the live LAN peer view published by the supervisor's
    /// `mDNS` browse while the LAN transport is active, read by the local-only
    /// `ListLanPeers` dispatch handler. `None` while the LAN transport is dormant
    /// or off, so the handler returns an empty list (fail-closed, mirroring the
    /// tailnet `ListRemotePeers`). Published by [`start_lan`](Self::start_lan) on
    /// activation and cleared by [`deactivate_lan`](Self::deactivate_lan); read
    /// behind a short `std::sync::Mutex` critical section off the `.await` path.
    lan_peer_handle: std::sync::Mutex<Option<LanPeerHandle>>,
}

/// The currently-bound remote listener, owned by the [`remote_supervisor`] task.
struct RemoteListenerState {
    running: Option<RunningRemoteListener>,
}

/// One bound listener generation: the port, the tailnet addresses actually
/// bound, and one accept task per address (aborting them closes the sockets).
struct RunningRemoteListener {
    port: u16,
    addrs: Vec<IpAddr>,
    accept_tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl RemoteControl {
    /// Create a fresh, stopped control handle. Pair it with a [`remote_supervisor`]
    /// task to actually drive the listeners. Both transports start disabled with
    /// their own admission caps and empty sever registries.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reload: tokio::sync::Notify::new(),
            next_conn_id: AtomicU64::new(0),
            tailnet: TransportControl::new(REMOTE_CONNECTION_CAP, REMOTE_PENDING_HANDSHAKE_CAP),
            lan: TransportControl::new(LAN_CONNECTION_CAP, LAN_PENDING_HANDSHAKE_CAP),
            pending_approvals: PendingApprovals::new(),
            trusted_devices: Arc::new(std::sync::Mutex::new(TrustedDevicesStore::load())),
            trusted_networks: Arc::new(std::sync::Mutex::new(TrustedNetworksStore::load())),
            lan_peer_handle: std::sync::Mutex::new(None),
        })
    }

    /// The per-transport control state for `transport`.
    fn transport_control(&self, transport: Transport) -> &TransportControl {
        match transport {
            Transport::Tailnet => &self.tailnet,
            Transport::Lan => &self.lan,
        }
    }

    /// Ask the supervisor to re-read the config and re-apply the listener state.
    /// Synchronous (no await) so callers on the per-connection dispatch path stay
    /// entirely off the apply call graph. Coalesced: bursts collapse to a single
    /// re-apply against the latest on-disk config.
    fn request_reload(&self) {
        self.reload.notify_one();
    }

    /// Register an accepted connection's sever channel on `transport`, returning
    /// its id — or `None` if that transport was disabled in the race between
    /// authorization and here, telling the caller to close instead of serve. The
    /// disable path fires every registered sever (see
    /// [`sever_all_connections`](Self::sever_all_connections)) so each live
    /// connection drops out of its message loop and runs the normal detach
    /// cleanup. Registered before serving begins so even a pre-Hello connection is
    /// signalable. A LAN connection passes its pinned `device_id` so a per-device
    /// revoke can later sever just it (FR-010); a tailnet connection passes `None`.
    async fn register_connection(
        &self,
        transport: Transport,
        sever: tokio::sync::oneshot::Sender<()>,
        device_id: Option<DeviceId>,
    ) -> Option<u64> {
        // Check the transport's `enabled` flag under the same lock its
        // `sever_all_connections` drains under. `disable` clears `enabled` (via
        // `stop`) before it drains, so a connection that raced a live disable
        // between authorization and here is either drained by that disable
        // (registered first) or refused registration (registered after) — it can
        // never linger past a disable.
        let control = self.transport_control(transport);
        let mut conns = control.connections.lock().await;
        if !control.enabled.load(Ordering::SeqCst) {
            return None;
        }
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        conns.insert(id, sever, device_id);
        Some(id)
    }

    /// Drop a connection's sever channel on `transport` once its task has ended. A
    /// no-op when the disable path already drained it.
    async fn deregister_connection(&self, transport: Transport, id: u64) {
        self.transport_control(transport).connections.lock().await.remove(id);
    }

    /// Bring the TAILNET listener into line with `cfg`: start it when enabled
    /// (bound strictly to this machine's tailnet addresses, never `0.0.0.0`),
    /// stop it when disabled, or rebind it when the port or address set changed.
    /// Idempotent when already matching. Fails closed — the listener stays down —
    /// on any tailnet `LocalAPI` error. Never restarts the server (research D8).
    /// `state` is the supervisor task's own tailnet listener state (no shared
    /// lock). Operates solely on the tailnet transport; the LAN transport is
    /// reconciled independently by [`apply_lan`](Self::apply_lan).
    async fn apply_tailnet(
        self: &Arc<Self>,
        state: &mut RemoteListenerState,
        cfg: &scribe_config::RemoteConfig,
        server: &IpcServerState,
    ) {
        if !cfg.enabled {
            self.disable(Transport::Tailnet, state).await;
            return;
        }

        // Enabled: enumerate this machine's tailnet bind addresses. Any LocalAPI
        // failure fails closed — there is no wildcard fallback (FR-002).
        let addrs = match crate::tailnet::bind_addresses().await {
            Ok(addrs) => addrs,
            Err(e) => {
                warn!(
                    "remote access enabled but tailnet address lookup failed ({e}); listener not started"
                );
                self.stop(Transport::Tailnet, state);
                return;
            }
        };
        if addrs.is_empty() {
            warn!(
                "remote access enabled but no tailnet addresses are available; listener not started"
            );
            self.stop(Transport::Tailnet, state);
            return;
        }

        // No-op when already bound to the same port + address set.
        if let Some(running) = &state.running
            && running.port == cfg.port
            && addr_set_matches(&running.addrs, &addrs)
        {
            return;
        }

        // First start, or a port/address change: tear the old listener down and
        // rebind fresh.
        self.stop(Transport::Tailnet, state);
        self.start(state, cfg.port, addrs, server).await;
    }

    /// Reconcile the LAN transport with the `[remote.lan]` config (feature 014
    /// T010). The LAN transport is present ONLY while `remote.lan.enabled` AND the
    /// machine is on a trusted network ([`network::lan_activation_snapshot`],
    /// FR-018); when active it binds one listener per physical-LAN address on
    /// `remote.lan.port` and advertises over mDNS, and when dormant (disabled,
    /// untrusted network, or an unidentifiable/keyring-less state) it tears the
    /// listener down and sends the mDNS goodbye. Started/stopped/rebound live off
    /// this path — driven by BOTH the `ConfigReloaded` reload and the network-change
    /// watcher poke (both funnel through [`request_reload`](Self::request_reload)) —
    /// never a server restart. Operates solely on the LAN transport; the tailnet
    /// transport is reconciled independently by [`apply_tailnet`](Self::apply_tailnet).
    async fn apply_lan(
        self: &Arc<Self>,
        state: &mut RemoteListenerState,
        cfg: &scribe_config::LanRemoteConfig,
        runtime: &mut LanRuntime,
        sup: LanSupervise<'_>,
    ) {
        // Off (the default): tear any LAN listener + advertising down.
        if !cfg.enabled {
            self.deactivate_lan(state, runtime, Some(LanDormantReason::Disabled)).await;
            return;
        }

        // Enabled: activate only on a trusted network, bound to that network's
        // physical-LAN address. Trust and the bind addresses come from ONE netdev
        // read so a roam can never leave them disagreeing; the blocking read runs
        // off the async runtime. Fails closed (dormant) when the network is
        // unidentifiable.
        let networks_for_snapshot = Arc::clone(sup.networks);
        let (trusted, addrs) = tokio::task::spawn_blocking(move || {
            networks_for_snapshot
                .lock()
                .ok()
                .map(|store| network::lan_activation_snapshot(&store))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        if !trusted {
            debug!("LAN access enabled but the current network is not trusted; staying dormant");
            self.deactivate_lan(state, runtime, Some(LanDormantReason::NetworkUntrusted)).await;
            return;
        }
        if addrs.is_empty() {
            warn!(
                "LAN access enabled on a trusted network but no physical-LAN address is available; listener not started"
            );
            self.deactivate_lan(state, runtime, None).await;
            return;
        }

        // Materialize the device identity lazily on first activation; fail closed
        // (dormant) if the keyring is unavailable — the private key is never written
        // in the clear and a keyring-less host cannot be an owning-side LAN host in
        // v1 (analysis I2).
        if let Err(error) = runtime.ensure_identity().await {
            warn!(%error, "LAN device identity unavailable; keeping the LAN transport dormant");
            self.deactivate_lan(state, runtime, None).await;
            return;
        }

        // No-op when already bound to the same port + physical-LAN address set —
        // advertising is already up (mirrors `apply_tailnet`).
        if let Some(running) = &state.running
            && running.port == cfg.port
            && addr_set_matches(&running.addrs, &addrs)
        {
            return;
        }

        // First activation, or a port/address change (a roam or DHCP renew):
        // rebind fresh and re-advertise. Live LAN connections are kept across a
        // rebind — only a full disable severs them — matching the tailnet path.
        // Assemble the per-connection accept context from the now-materialized
        // identity (a defensive fail-closed if it is somehow still absent).
        let Some(identity) = runtime.identity.clone() else {
            warn!("LAN listener start requested without a device identity; staying dormant");
            self.deactivate_lan(state, runtime, None).await;
            return;
        };
        let accept = self.lan_accept_context(identity, sup);
        self.stop(Transport::Lan, state);
        self.start_lan(state, LanBind { port: cfg.port, addrs }, runtime, accept).await;
    }

    /// Assemble the per-connection LAN accept context (feature 014 T011) from this
    /// machine's materialized device identity plus the shared trust/network state:
    /// the mutual-TLS layer (presenting this device's cert, pinning peers against
    /// the LIVE trusted-device store), the server state threaded into each
    /// accepted connection's dispatch, and the trusted-networks store (for the
    /// approval prompt's network label). Cheap to clone per accepted connection.
    fn lan_accept_context(
        self: &Arc<Self>,
        identity: Arc<DeviceIdentity>,
        sup: LanSupervise<'_>,
    ) -> LanAccept {
        let pins: Arc<dyn DevicePins> =
            Arc::new(TrustedDevicePins(Arc::clone(&self.trusted_devices)));
        LanAccept {
            server: sup.server.clone(),
            control: Arc::clone(self),
            lan_tls: Arc::new(LanTls::new(identity, pins)),
            networks: Arc::clone(sup.networks),
        }
    }

    /// Take the LAN transport dormant: tear its listener down and sever its live
    /// connections (via the generalized per-transport [`disable`](Self::disable) —
    /// only LAN connections, never the tailnet transport's), then stop advertising
    /// and send the mDNS goodbye. Idempotent — safe on every reload while the
    /// transport stays off. Emits the bulk `lan: dormant …` audit line
    /// (contracts/settings-and-config.md) once, only on a genuine active→dormant
    /// transition (a live listener was actually torn down) and only for the two
    /// user-meaningful `reason`s; an idempotent no-op reload, or a fail-closed
    /// operational case that passes `None`, stays silent.
    async fn deactivate_lan(
        &self,
        state: &mut RemoteListenerState,
        runtime: &mut LanRuntime,
        reason: Option<LanDormantReason>,
    ) {
        let was_active = state.running.is_some();
        self.disable(Transport::Lan, state).await;
        runtime.stop_advertising();
        // Clear the published peer view so a dormant/disabled LAN reports no peers.
        self.publish_lan_peers(None);
        // Bulk transition line (the LAN counterpart to the tailnet `remote: severed
        // …` line `disable` emits): fire only when a live listener was actually torn
        // down, so a repeated dormant reload never spams it.
        if let Some(reason) = reason
            && was_active
        {
            info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: dormant reason={}",
                audit_dormant_reason(reason)
            );
        }
    }

    /// Publish (or, with `None`, clear) the LAN peer view the local-only
    /// `ListLanPeers` handler reads. [`start_lan`](Self::start_lan) publishes the
    /// active discovery's [`LanPeerHandle`] once browsing is up, and
    /// [`deactivate_lan`](Self::deactivate_lan) clears it on dormancy so a dormant
    /// LAN reports no peers. Runs only in the single supervisor task; the sync lock
    /// is taken and released without an `.await` held.
    fn publish_lan_peers(&self, handle: Option<LanPeerHandle>) {
        if let Ok(mut slot) = self.lan_peer_handle.lock() {
            *slot = handle;
        }
    }

    /// A snapshot of the LAN peers currently discovered on the trusted network,
    /// for the local-only `ListLanPeers` handler. Empty while the LAN transport is
    /// dormant or off (nothing is being browsed), or on a poisoned lock — the same
    /// fail-closed empty-on-unavailable behavior as the tailnet `ListRemotePeers`.
    /// The critical section is a short clone off the `.await` path.
    fn lan_peers(&self) -> Vec<LanPeerInfo> {
        match self.lan_peer_handle.lock() {
            Ok(slot) => slot.as_ref().map_or_else(Vec::new, LanPeerHandle::peers),
            Err(_poisoned) => Vec::new(),
        }
    }

    /// Feature 014 T027: this machine's approved LAN devices for the local-only
    /// `ListTrustedDevices` handler, read from the shared trusted-device pin store
    /// (the same store the accept path's approval gate writes and
    /// [`revoke_trusted_device`](Self::revoke_trusted_device) removes from). A
    /// short in-memory clone off the `.await` path — the store's `list` touches no
    /// disk. A poisoned lock recovers the guard (the record set is still
    /// consistent — every mutation rebuilds the whole document), matching how the
    /// accept path locks this same store.
    fn list_trusted_devices(&self) -> Vec<TrustedDeviceInfo> {
        self.trusted_devices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).list()
    }

    /// Feature 014 T021: this machine's trusted networks plus whether its CURRENT
    /// network is one of them, for the local-only `ListTrustedNetworks` handler
    /// (`current_trusted` drives the Settings active/dormant status line, UX-004).
    /// The pure list read and the current-network trust check (a blocking `netdev`
    /// read) run together on the blocking pool under one short store lock — the
    /// same lock-across-`netdev`-in-`spawn_blocking` shape
    /// [`apply_lan`](Self::apply_lan) uses for its activation snapshot. Fails closed
    /// (empty list, untrusted) on a poisoned lock or a join failure.
    async fn trusted_networks_snapshot(&self) -> (Vec<TrustedNetworkInfo>, bool) {
        let networks = Arc::clone(&self.trusted_networks);
        tokio::task::spawn_blocking(move || match networks.lock() {
            Ok(store) => (store.list(), store.is_current_network_trusted()),
            Err(_poisoned) => (Vec::new(), false),
        })
        .await
        .unwrap_or_default()
    }

    /// Feature 014 T021: trust the network this machine is currently on, then poke
    /// the supervisor so [`apply_lan`](Self::apply_lan) re-evaluates trust and the
    /// (enabled) LAN transport can go ACTIVE now that a matching trusted network
    /// exists — no config change or server restart. Fingerprinting the current
    /// network is a blocking `netdev` read and the store rewrite fsyncs to disk, so
    /// the work runs on the blocking pool. Fire-and-forget on the wire (no reply
    /// frame): an unidentifiable network, a poisoned lock, or a persist failure is
    /// logged and no reload is poked — the Settings UI's disabled "Add current
    /// network" control already pre-empts the unidentifiable case.
    async fn add_current_trusted_network(&self) {
        let networks = Arc::clone(&self.trusted_networks);
        let added = tokio::task::spawn_blocking(move || {
            let mut store = match networks.lock() {
                Ok(store) => store,
                Err(_poisoned) => {
                    warn!("trusted-networks store lock poisoned; not adding the network");
                    return false;
                }
            };
            match store.add_current(None) {
                Ok(_info) => true,
                Err(error) => {
                    warn!(%error, "failed to trust the current network");
                    false
                }
            }
        })
        .await
        .unwrap_or(false);
        if added {
            self.request_reload();
        }
    }

    /// Feature 014 T021: remove a trusted network by its record id, then poke the
    /// supervisor. Removing the CURRENT network must take the LAN surface dormant
    /// promptly (FR-018): the reload makes [`apply_lan`](Self::apply_lan) re-evaluate
    /// trust and, when the machine no longer matches any trusted network, tear the
    /// LAN listener down, send the `mDNS` goodbye, and sever ONLY the LAN transport's
    /// connections (never the tailnet's). Removing a non-current network changes
    /// nothing live and the reload is idempotent. The store rewrite fsyncs to disk,
    /// so it runs on the blocking pool. Fire-and-forget on the wire; a poisoned lock
    /// or persist failure is logged and no reload is poked.
    async fn remove_trusted_network(&self, id: String) {
        let networks = Arc::clone(&self.trusted_networks);
        let removed = tokio::task::spawn_blocking(move || {
            let mut store = match networks.lock() {
                Ok(store) => store,
                Err(_poisoned) => {
                    warn!("trusted-networks store lock poisoned; not removing the network");
                    return false;
                }
            };
            match store.remove(&id) {
                Ok(removed) => removed,
                Err(error) => {
                    warn!(%error, "failed to remove the trusted network");
                    false
                }
            }
        })
        .await
        .unwrap_or(false);
        if removed {
            self.request_reload();
        }
    }

    /// Feature 014 T027: revoke a trusted LAN device by its hex `device_id`, then
    /// sever ONLY that device's live LAN connection(s) so it loses control at once
    /// and must re-approve on its next attempt (FR-010, SC-006). Two ordered
    /// steps, both fail-closed:
    ///
    /// 1. Remove the pin from the trusted-device store (a disk fsync, so it runs on
    ///    the blocking pool). Done FIRST so any reconnect that races the sever is
    ///    already treated as an unknown device (re-approval required), never
    ///    silently re-admitted.
    /// 2. Fire each matched connection's sever oneshot via the T009
    ///    `device_id -> connection-id` index, dropping it out of its message loop
    ///    through the normal detach path — the owning sessions and every OTHER
    ///    device's connections untouched.
    ///
    /// A malformed hex id, an unknown/already-revoked device, or a poisoned store
    /// is a logged no-op for the pin removal; the sever still runs (idempotent, and
    /// correct even in the rare approved-but-unpersisted race). Fire-and-forget on
    /// the wire — no reply frame (the Settings UI re-queries the list afterward).
    async fn revoke_trusted_device(&self, device_id_hex: String) {
        let Some(device_id) = decode_device_id_hex(&device_id_hex) else {
            warn!("ignoring RevokeTrustedDevice with a malformed device id");
            return;
        };

        // Step 1: remove the pin (blocking fsync off the async runtime), capturing
        // the device label first for the audit line. `Some(label)` iff a record was
        // actually removed; `None` on an unknown device or a store failure.
        let devices = Arc::clone(&self.trusted_devices);
        let revoked_label = tokio::task::spawn_blocking(move || {
            let mut store = devices.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let label = store.label_for(&device_id).unwrap_or_default();
            match store.revoke(&device_id) {
                Ok(true) => Some(label),
                Ok(false) => None,
                Err(error) => {
                    warn!(%error, "failed to revoke the trusted device");
                    None
                }
            }
        })
        .await
        .unwrap_or(None);

        // Step 2: sever the device's live LAN connection(s) via the T009 device
        // index so a revoked device drops immediately (SC-006).
        let severed = self.sever_device_connections(&device_id).await;

        if let Some(label) = revoked_label {
            info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: revoked device={label} id={}",
                short_device_id(&device_id)
            );
        }
        if severed > 0 {
            debug!(count = severed, "severed live LAN connections for a revoked device");
        }
    }

    /// Bind one LAN listener per physical-LAN address on `port`, spawn its accept
    /// loop, mark the LAN transport enabled, and advertise over mDNS — the LAN
    /// counterpart to [`start`](Self::start). Sets the LAN transport's `enabled`
    /// only if at least one address bound. Feature 014 T010 owns the listener
    /// lifecycle here; each accepted connection's `tokio-rustls` mutual handshake,
    /// device-approval gate, and hand-off to [`serve_connection`] run in
    /// [`lan_accept_loop`] → [`handle_lan_client`] (T011).
    async fn start_lan(
        &self,
        state: &mut RemoteListenerState,
        bind: LanBind,
        runtime: &mut LanRuntime,
        accept: LanAccept,
    ) {
        let LanBind { port, addrs } = bind;
        let mut accept_tasks = Vec::new();
        let mut bound_addrs = Vec::new();
        for ip in addrs {
            let sockaddr = SocketAddr::new(ip, port);
            match TcpListener::bind(sockaddr).await {
                Ok(listener) => {
                    accept_tasks.push(tokio::spawn(lan_accept_loop(listener, accept.clone())));
                    bound_addrs.push(ip);
                    info!(%sockaddr, "LAN remote-control listener bound");
                }
                Err(e) => {
                    warn!(%sockaddr, "failed to bind LAN remote-control listener: {e}");
                }
            }
        }
        if accept_tasks.is_empty() {
            warn!(
                "LAN access enabled and trusted but every physical-LAN bind failed; LAN inactive"
            );
            return;
        }
        // Publish the accepting state only once a listener is actually up, so the
        // disable-race check never reports `enabled` for a down listener.
        self.lan.enabled.store(true, Ordering::SeqCst);
        // Advertise the bound physical-LAN address(es) so peers discover this host
        // only while the transport is genuinely active (FR-018).
        runtime.advertise(&bound_addrs, port);
        // Publish the now-browsing peer view so the local-only `ListLanPeers`
        // handler reads live discovery; a missing handle (mDNS unavailable) leaves
        // it cleared, and the handler then reports no peers.
        self.publish_lan_peers(runtime.peer_handle());
        state.running = Some(RunningRemoteListener { port, addrs: bound_addrs, accept_tasks });
    }

    /// Bind one tailnet listener per address on `port` and spawn its accept task.
    /// Sets the tailnet transport's `enabled` only if at least one address bound.
    async fn start(
        self: &Arc<Self>,
        state: &mut RemoteListenerState,
        port: u16,
        addrs: Vec<IpAddr>,
        server: &IpcServerState,
    ) {
        let mut accept_tasks = Vec::new();
        let mut bound_addrs = Vec::new();
        for ip in addrs {
            let sockaddr = SocketAddr::new(ip, port);
            match TcpListener::bind(sockaddr).await {
                Ok(listener) => {
                    let server = server.clone();
                    let control = Arc::clone(self);
                    accept_tasks.push(tokio::spawn(remote_accept_loop(listener, server, control)));
                    bound_addrs.push(ip);
                    info!(%sockaddr, "remote-control listener bound");
                }
                Err(e) => {
                    warn!(%sockaddr, "failed to bind remote-control listener: {e}");
                }
            }
        }
        if accept_tasks.is_empty() {
            warn!("remote access enabled but every tailnet bind failed; remote access inactive");
            return;
        }
        // Publish the accepting state only once a listener is actually up, so the
        // disable-race check never reports `enabled` for a down listener.
        self.tailnet.enabled.store(true, Ordering::SeqCst);
        state.running = Some(RunningRemoteListener { port, addrs: bound_addrs, accept_tasks });
    }

    /// Stop accepting on `transport`'s listener: clear that transport's `enabled`
    /// first (so an in-flight handler racing a disable answers `Disabled`), then
    /// abort every accept task in `state`, which drops the listeners and closes
    /// their sockets. Live connections are severed separately by
    /// [`disable`](Self::disable) via
    /// [`sever_all_connections`](Self::sever_all_connections); this tears down
    /// only the accept side (shared with the rebind path, which keeps live
    /// connections). Only the named transport's `enabled` flag and listener state
    /// are touched.
    fn stop(&self, transport: Transport, state: &mut RemoteListenerState) {
        self.transport_control(transport).enabled.store(false, Ordering::SeqCst);
        if let Some(running) = state.running.take() {
            for task in running.accept_tasks {
                task.abort();
            }
            info!(
                port = running.port,
                addrs = running.addrs.len(),
                "remote-control listener stopped"
            );
        }
    }

    /// Disable `transport` (research D8, FR-016): tear its listener down, then
    /// sever every live connection ON THAT TRANSPORT and emit the bulk `severed`
    /// audit line. Idempotent — a redundant disable with nothing live is a silent
    /// no-op, so it is safe on every `ConfigReloaded` while the transport stays
    /// off. Owning-side sessions are never touched: each connection is severed
    /// through its normal detach path (FR-010). Only the named transport's
    /// listener + connections are affected; the other transport is untouched.
    async fn disable(&self, transport: Transport, state: &mut RemoteListenerState) {
        // Stop accepting and close the listener sockets first (no new connections),
        // then sever the connections that are already established — only this
        // transport's, via its own sever registry.
        self.stop(transport, state);
        let severed = self.sever_all_connections(transport).await;
        // The tailnet `severed` audit line is preserved byte-for-byte (feature
        // 013); the LAN transport's dormant/disable audit is added with the LAN
        // audit surface (feature 014 T022).
        if severed > 0 && transport == Transport::Tailnet {
            info!(
                target: REMOTE_AUDIT_TARGET,
                "remote: severed reason={}",
                audit_reason(RemoteRefusal::Disabled)
            );
            debug!(count = severed, "severed live remote connections on disable");
        }
    }

    /// Fire the sever signal for every registered connection on `transport`. Each
    /// signaled connection drops out of its message loop, sends its best-effort
    /// `ServerMessage::RemoteDisconnect`, runs the normal detach cleanup (owning
    /// sessions untouched, FR-016/FR-010), and closes — all on its own task, so a
    /// wedged link cannot stall the others or this disable. Only the named
    /// transport's connections are touched. Returns how many connections were
    /// signaled (used to gate the bulk `severed` audit line).
    async fn sever_all_connections(&self, transport: Transport) -> usize {
        // Drain the registry under the lock, then signal with the lock released so
        // nothing is held across the per-connection sends.
        let severs = {
            let mut conns = self.transport_control(transport).connections.lock().await;
            conns.drain_all()
        };
        let count = severs.len();
        for sever in severs {
            // The receiver lives in the connection's serve task until it tears
            // down, so this normally delivers; a receiver already gone (task ending
            // on its own) makes `send` return `Err` — a harmless no-op.
            if sever.send(()).is_err() {
                debug!("remote sever target already gone");
            }
        }
        count
    }

    /// Fire the sever signal for every live LAN connection owned by `device_id`,
    /// returning how many were signaled. The per-device counterpart to
    /// [`sever_all_connections`](Self::sever_all_connections): it drains only the
    /// matched connections from the LAN transport's `device_id -> connection-id`
    /// index (T009) so a revoked device drops out of its message loop through the
    /// normal detach path while every other connection and the owning sessions stay
    /// live (FR-010, SC-006). Only the LAN transport carries a device index; the
    /// tailnet transport is never consulted. The registry is drained under its lock
    /// and each sever is sent with the lock released, mirroring
    /// [`sever_all_connections`](Self::sever_all_connections).
    async fn sever_device_connections(&self, device_id: &DeviceId) -> usize {
        let severs = {
            let mut conns = self.lan.connections.lock().await;
            conns.drain_device(device_id)
        };
        let count = severs.len();
        for sever in severs {
            // Same harmless race as the bulk sever: a receiver whose serve task is
            // already tearing down makes `send` return `Err`.
            if sever.send(()).is_err() {
                debug!("LAN revoke sever target already gone");
            }
        }
        count
    }
}

/// The LAN transport's supervisor-owned runtime, built lazily on first
/// activation and carried across apply cycles (feature 014 T010). It holds the
/// persistent per-install [`DeviceIdentity`] — materialized only when the LAN
/// transport actually goes active, since generating/sealing it needs an
/// interactive keyring (analysis I2) — and the active mDNS [`LanDiscovery`]
/// handle, which is torn down (sending an mDNS goodbye) whenever the transport
/// goes dormant. Lives in the [`remote_supervisor`] task, off the shared
/// [`RemoteControl`], so nothing here is touched by the dispatch or accept paths.
#[derive(Default)]
struct LanRuntime {
    /// The device identity, generated and keyring-sealed on first activation and
    /// then kept for the process. `None` until the LAN transport first goes
    /// active. Held in an `Arc` so the per-listener [`LanTls`] can share it
    /// cheaply across every accepted connection's handshake.
    identity: Option<Arc<DeviceIdentity>>,
    /// The mDNS advertise + browse handle, present only while the transport is
    /// active. Dropping it sends the mDNS goodbye and shuts the daemon down.
    discovery: Option<LanDiscovery>,
}

impl LanRuntime {
    /// Ensure the device identity exists, generating and sealing it on first use.
    /// Fails closed (leaving `identity` `None`) when the state dir or keyring is
    /// unavailable, so the caller keeps the LAN transport dormant.
    async fn ensure_identity(&mut self) -> Result<(), crate::lan::identity::IdentityError> {
        if self.identity.is_none() {
            self.identity = Some(Arc::new(crate::lan::identity::load_or_generate().await?));
        }
        Ok(())
    }

    /// Advertise this machine on the LAN over mDNS with the bound physical-LAN
    /// address(es), creating the discovery daemon on first use. Idempotent:
    /// re-advertises with the supplied address set/port, so a live rebind refreshes
    /// the advert. A no-op when the identity is not yet materialized or the mDNS
    /// daemon cannot be created (logged; the listener still runs, discovery is a
    /// convenience layer).
    fn advertise(&mut self, addrs: &[IpAddr], port: u16) {
        let Some(device_id_hex) = self.identity.as_ref().map(|identity| identity.device_id_hex())
        else {
            return;
        };
        if self.discovery.is_none() {
            match LanDiscovery::new(device_id_hex.clone()) {
                Ok(discovery) => self.discovery = Some(discovery),
                Err(error) => {
                    warn!(%error, "LAN mDNS discovery unavailable; not advertising");
                    return;
                }
            }
        }
        let Some(discovery) = self.discovery.as_ref() else {
            return;
        };
        let config = AdvertiseConfig {
            device_id_hex,
            host: local_hostname(),
            addrs: addrs.to_vec(),
            port,
            protocol_version: scribe_common::protocol::REMOTE_PROTOCOL_VERSION,
        };
        if let Err(error) = discovery.start_advertising(&config) {
            warn!(%error, "failed to advertise Scribe on the LAN");
        }
        if let Err(error) = discovery.start_browsing() {
            warn!(%error, "failed to start LAN peer browsing");
        }
    }

    /// Stop advertising and tear the mDNS daemon down when going dormant. Dropping
    /// [`LanDiscovery`] sends the mDNS goodbye, aborts browsing, and shuts the
    /// daemon down (see its `Drop`); the persistent device identity is retained for
    /// the next activation.
    fn stop_advertising(&mut self) {
        self.discovery = None;
    }

    /// A read handle over the live `mDNS` peer table when discovery is active, for
    /// the supervisor to publish into the shared [`RemoteControl`] (feature 014
    /// T013). `None` when discovery has not been created (`mDNS` unavailable, or the
    /// transport is not advertising), so the `ListLanPeers` handler reports no peers.
    fn peer_handle(&self) -> Option<LanPeerHandle> {
        self.discovery.as_ref().map(LanDiscovery::peer_handle)
    }
}

/// Owns the remote-control listener lifecycle off the per-connection dispatch
/// graph. Spawned once at server startup with its own [`RemoteListenerState`]:
/// it applies the current `[remote]` config immediately (a no-op when disabled —
/// the default), then re-applies on every [`RemoteControl::request_reload`]
/// notification (fired by the `ConfigReloaded` path). Re-reading the config here
/// serializes every start/stop/rebind through one task in notify order, so the
/// server is never restarted and concurrent reloads can never race.
///
/// Both startup paths — a fresh `run_normal_server` and a post-upgrade
/// `run_upgrade_receiver` — funnel through the same `run_server_loop`, so this
/// supervisor also runs after a hot-reload handoff and re-derives BOTH the
/// tailnet and LAN listeners purely from config (feature 013 T032; feature 014
/// T029). A handoff carries no remote/LAN listener or connection state, no
/// device identity, and no trust stores (see [`crate::handoff::HandoffState`]):
/// the LAN transport re-materializes its device identity from the keyring/disk
/// via [`LanRuntime::ensure_identity`] and reads the trust stores that
/// [`RemoteControl::new`] reloads from disk, so the keypair on disk/keyring need
/// not cross the wire and the reconstituted server keeps the SAME pinned
/// identity. The old server's remote/LAN connections drop when it exits and the
/// client auto-reconnects to the rebound listener (research D6).
pub async fn remote_supervisor(control: Arc<RemoteControl>, server: IpcServerState) {
    // One listener state per transport (feature 014): the tailnet and LAN
    // listeners are reconciled independently each cycle so a change to one never
    // disturbs the other.
    let mut tailnet_state = RemoteListenerState { running: None };
    let mut lan_state = RemoteListenerState { running: None };
    // The LAN transport's lazily-built runtime (device identity + mDNS handle),
    // owned by this task so the identity is only materialized once the LAN
    // transport actually goes active.
    let mut lan_runtime = LanRuntime::default();
    // The trusted-networks store backs the LAN activation gate (`apply_lan`), the
    // network-change watcher, AND the local-only add/remove/list handlers, so it
    // lives on the shared `RemoteControl` (loaded from disk in `RemoteControl::new`,
    // re-derived on a post-handoff start): a live add/remove and the gate observe
    // ONE store, so removing the current network pokes a reload that goes dormant.
    //
    // Wire the network-change watcher to the supervisor: when the trusted-network
    // status flips (a roam), it pokes a reload so `apply_lan` re-evaluates and the
    // LAN transport goes dormant/active promptly — NOT only on a config reload
    // (analysis C5, FR-018/SC-007). The `false` baseline is safe: a spurious first
    // poke coalesces into the next apply, which is idempotent. Held for the life of
    // the (never-returning) supervisor task.
    let _network_watcher = {
        let control_for_poke = Arc::clone(&control);
        network::spawn_network_watcher(
            Arc::clone(&control.trusted_networks),
            network::DEFAULT_NETWORK_POLL_INTERVAL,
            false,
            move |_trusted_now| control_for_poke.request_reload(),
        )
    };
    loop {
        match crate::config::load_config() {
            Ok(cfg) => {
                control.apply_tailnet(&mut tailnet_state, &cfg.remote, &server).await;
                control
                    .apply_lan(
                        &mut lan_state,
                        &cfg.remote.lan,
                        &mut lan_runtime,
                        LanSupervise { networks: &control.trusted_networks, server: &server },
                    )
                    .await;
            }
            Err(e) => {
                warn!("remote supervisor: config load failed ({e}); listeners left unchanged");
            }
        }
        // Wait for the next config change or network-trust flip. `Notify` remembers
        // a poke that arrives mid-apply, so no reload is missed.
        control.reload.notified().await;
    }
}

/// Whether two tailnet address lists cover the same set (order-independent).
fn addr_set_matches(a: &[IpAddr], b: &[IpAddr]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let set: HashSet<IpAddr> = a.iter().copied().collect();
    b.iter().all(|ip| set.contains(ip))
}

/// Accept loop for one bound tailnet listener. Spawned per address by
/// [`RemoteControl::start`] and aborted on disable/rebind. Every accepted stream
/// is handed to [`handle_remote_client`], which runs the preamble.
async fn remote_accept_loop(
    listener: TcpListener,
    server: IpcServerState,
    control: Arc<RemoteControl>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                // Admission control (FR-013 hardening): reserve a pending-handshake
                // permit the instant the stream is accepted — BEFORE spawning any
                // handler or reading a byte — so a flood of half-open dialers cannot
                // spawn unbounded pre-auth work (mirrors `start_ipc_server`). Excess
                // dialers are dropped immediately; the stream closes with the scope.
                let Ok(handshake_permit) =
                    Arc::clone(&control.tailnet.handshake_limit).try_acquire_owned()
                else {
                    debug!(
                        %peer_addr,
                        cap = REMOTE_PENDING_HANDSHAKE_CAP,
                        "pending-handshake admission cap reached; dropping remote connection"
                    );
                    continue;
                };
                // Match the local socket's latency profile for keystroke traffic.
                if let Err(e) = stream.set_nodelay(true) {
                    debug!(%peer_addr, "failed to set TCP_NODELAY on remote connection: {e}");
                }
                // Defense-in-depth: OS keepalive so a vanished peer is eventually
                // detected at the TCP layer (the app idle-read timeout is the
                // primary reclamation — see `REMOTE_IDLE_READ_TIMEOUT`).
                enable_tcp_keepalive(&stream, peer_addr);
                let server = server.clone();
                let control = Arc::clone(&control);
                tokio::spawn(async move {
                    // `Box::pin` the large handshake future so it lives on the heap
                    // rather than bloating this spawned task's stack frame.
                    Box::pin(handle_remote_client(stream, peer_addr, server, control)).await;
                    drop(handshake_permit);
                });
            }
            Err(e) => {
                error!("remote accept error: {e}");
                // A network listener can surface persistent accept errors (e.g.
                // EMFILE); a brief pause avoids a hot error loop.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
}

/// The supervisor-owned cross-cutting dependencies threaded into the LAN
/// reconcile path beyond the listener state + config: the shared trusted-networks
/// store and the IPC server state. Borrowed as one `Copy` handle so
/// [`RemoteControl::apply_lan`] stays under Clippy's argument threshold (mirrors
/// [`ConnState`]).
#[derive(Clone, Copy)]
struct LanSupervise<'a> {
    networks: &'a SharedTrustedNetworks,
    server: &'a IpcServerState,
}

/// One LAN listener generation's bind target — the port plus the physical-LAN
/// addresses to bind. Bundled so [`RemoteControl::start_lan`] stays under
/// Clippy's argument threshold.
struct LanBind {
    port: u16,
    addrs: Vec<IpAddr>,
}

/// The per-connection LAN accept context (feature 014 T011): everything the
/// accept loop + handler need to run a mutual-TLS handshake, gate approval, and
/// hand off to [`serve_connection`]. Built once per listener generation by
/// [`RemoteControl::lan_accept_context`] and cloned per accepted connection (all
/// fields are `Arc`/`Clone`, so cloning is cheap). Bundled so the accept loop,
/// handler, and gate stay under Clippy's argument threshold.
#[derive(Clone)]
struct LanAccept {
    /// IPC server state threaded into the shared 013 dispatch and used to raise
    /// the approval prompt on the owning machine's own local clients.
    server: IpcServerState,
    /// The remote-control supervisor handle: the LAN admission caps, sever
    /// registry + `device_id` index, and the pending-approval + trusted-device
    /// stores.
    control: Arc<RemoteControl>,
    /// The mutual-TLS layer presenting this device's cert and pinning peers by
    /// `device_id` against the live trusted-device store.
    lan_tls: Arc<LanTls>,
    /// The trusted-networks store, read for the approval prompt's network label.
    networks: SharedTrustedNetworks,
}

/// Adapts the shared trusted-device store to the TLS layer's [`DevicePins`]
/// pin-check trait so the SPKI-pinning verifier classifies each handshaking peer
/// as known vs. pending against the LIVE store — every approval/revoke visible on
/// the next handshake with no rebind. The check is a single non-blocking store
/// lookup that never spans an `.await`, honoring the trait's synchronous-handshake
/// contract.
struct TrustedDevicePins(SharedTrustedDevices);

impl DevicePins for TrustedDevicePins {
    fn is_pinned(&self, device_id: &DeviceId) -> bool {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_trusted(device_id)
    }
}

/// Accept loop for one bound LAN (mutual-TLS) listener (feature 014 T011).
/// Spawned per physical-LAN address by [`RemoteControl::start_lan`] and aborted
/// on disable/dormancy or a rebind (dropping the listener + closing its socket).
/// Every accepted stream reserves the LAN transport's OWN pending-handshake
/// permit the instant it is accepted — before any TLS work — so a flood of
/// half-open TLS dialers cannot spawn unbounded pre-auth tasks (mirrors the
/// tailnet [`remote_accept_loop`]); the permit is held for the connection's life.
async fn lan_accept_loop(listener: TcpListener, accept: LanAccept) {
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                // Reserve a LAN pending-handshake permit up front (analysis
                // C4/S1); excess dialers are dropped as the stream closes.
                let Ok(handshake_permit) =
                    Arc::clone(&accept.control.lan.handshake_limit).try_acquire_owned()
                else {
                    debug!(
                        %peer_addr,
                        cap = LAN_PENDING_HANDSHAKE_CAP,
                        "LAN pending-handshake admission cap reached; dropping connection"
                    );
                    continue;
                };
                // Match the local socket's latency profile for keystroke traffic
                // and enable OS keepalive so a vanished peer is eventually torn
                // down at the TCP layer (mirrors the tailnet accept loop).
                if let Err(e) = stream.set_nodelay(true) {
                    debug!(%peer_addr, "failed to set TCP_NODELAY on LAN connection: {e}");
                }
                enable_tcp_keepalive(&stream, peer_addr);
                let accept = accept.clone();
                tokio::spawn(async move {
                    // `Box::pin` the large handshake future so it lives on the heap
                    // rather than bloating this spawned task's stack frame.
                    Box::pin(handle_lan_client(stream, peer_addr, accept)).await;
                    drop(handshake_permit);
                });
            }
            Err(e) => {
                error!("LAN remote accept error: {e}");
                // A network listener can surface persistent accept errors (e.g.
                // EMFILE); a brief pause avoids a hot error loop.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
}

/// Whether the LAN device-approval gate cleared a connection to proceed to the
/// version gate + attach, or refused it (the `LanApprovalResult` refusal has
/// already been written, so the caller just closes).
enum LanGate {
    Proceed,
    Refused,
}

/// Handle one accepted LAN (mutual-TLS) connection per contracts/lan-protocol.md.
/// Sequence: `tokio-rustls` mutual handshake (SPKI-pinning verifier) → read the
/// `LanHello` preamble → device-approval gate (a pinned device proceeds; an
/// unknown device is held pending the owning user's explicit decision, revealing
/// NO window/session data) → exact protocol-version gate → hand the encrypted
/// stream to the shared 013 [`serve_connection`] dispatch with a bounded
/// `ClientSink::Remote` queue and a `Remote(device label)` controller. The
/// connection is registered in the LAN sever registry + `device_id` index so a
/// per-device revoke / per-transport disable can sever just it (FR-010/FR-012). A
/// failed TLS handshake or a malformed preamble closes bare (no channel to
/// answer). The `lan: accepted`/`disconnect` audit lines are emitted by
/// `serve_connection`; refusals log `lan: refused` here.
async fn handle_lan_client(stream: TcpStream, peer_addr: SocketAddr, accept: LanAccept) {
    // 1. Mutual-TLS handshake. A failure (reset, unparseable cert, bad handshake
    //    signature) has no secure channel to answer on — close bare, no audit
    //    (mirrors the tailnet bare-close on a malformed preamble).
    let (tls_stream, peer) = match accept.lan_tls.accept(stream).await {
        Ok(accepted) => accepted,
        Err(error) => {
            debug!(%peer_addr, %error, "LAN TLS handshake failed; closing");
            return;
        }
    };
    let (mut reader, mut write_half) = tokio::io::split(tls_stream);

    // 2. `LanHello` preamble (bounded + timed). A malformed / non-`LanHello` /
    //    timed-out first frame closes bare.
    let Some((device_name, client_version)) = read_lan_preamble(&mut reader).await else {
        return;
    };

    // 3. Device-approval gate (contract step 4). A pinned device proceeds; an
    //    unknown device is held pending the owning user's decision, revealing NO
    //    data (SEC-001). A refusal is already written by the gate.
    if let LanGate::Refused = lan_approval_gate(&accept, &peer, &device_name, &mut write_half).await
    {
        return;
    }

    // 4. Exact protocol-version gate (contract step 5, research D3).
    if client_version != REMOTE_PROTOCOL_VERSION {
        refuse_lan(&mut write_half, &device_name, LanRefusal::IncompatibleVersion).await;
        return;
    }

    // 5. Connection cap — the LAN transport's OWN slot, wholly independent of
    //    tailnet (analysis C4/S1). Held for the connection's life.
    let Ok(permit) = Arc::clone(&accept.control.lan.conn_limit).try_acquire_owned() else {
        refuse_lan(&mut write_half, &device_name, LanRefusal::Busy).await;
        return;
    };

    // 6. Register the sever channel under the LAN `device_id` index BEFORE
    //    admitting, re-checking the disable flag under the sever lock so a
    //    connection that raced a live disable is refused (`Disabled`) instead of
    //    admitted then silently dropped (FR-016; mirrors the tailnet path).
    let (sever_tx, sever_rx) = tokio::sync::oneshot::channel();
    let Some(conn_id) =
        accept.control.register_connection(Transport::Lan, sever_tx, Some(peer.device_id)).await
    else {
        refuse_lan(&mut write_half, &device_name, LanRefusal::Disabled).await;
        drop(permit);
        return;
    };

    // 7. Admit: tell the dialer to proceed to `Hello`, then drive the shared 013
    //    dispatch over the encrypted stream. The bounded output queue (T029) keeps
    //    a slow LAN link off the fan-out hot path exactly as for tailnet.
    if !send_lan_result(&mut write_half, true, None).await {
        // The dialer vanished before it could be admitted; unwind the slot.
        accept.control.deregister_connection(Transport::Lan, conn_id).await;
        drop(permit);
        return;
    }
    let (sink, drain_task) = spawn_remote_output(write_half, accept.server.clone());
    let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::Remote(sink.clone())));
    let ctx = RemoteContext {
        node_name: device_name,
        login_name: String::new(),
        audit: RemoteAudit::Lan { device_id_short: short_device_id(&peer.device_id) },
        sever: sever_rx,
    };
    // `serve_connection`/`finish_served_connection` emit the `lan: accepted …` and
    // `lan: disconnect …` audit lines via the `RemoteAudit::Lan` branch.
    serve_connection(reader, writer, accept.server.clone(), Some(ctx)).await;
    // Flush any final frame (e.g. the sever `RemoteDisconnect`), stop the drain
    // task, then release the sever registration + connection slot.
    shutdown_remote_output(&sink, drain_task).await;
    accept.control.deregister_connection(Transport::Lan, conn_id).await;
    drop(permit);
}

/// The LAN device-approval gate (contract step 4). A pinned device is already
/// trusted and proceeds immediately. An unknown device reserves a bounded, timed
/// pending-approval hold, tells the dialer it is waiting (`LanApprovalPending`),
/// raises the prompt on the owning machine's own local client(s)
/// (`LanApprovalRequest`), and holds with NO window/session data until the owning
/// user decides or the hold times out. On approve it persists a `TrustedDevice`
/// and proceeds; on decline/timeout (or a full pending cap) it writes the
/// `LanApprovalResult` refusal and returns [`LanGate::Refused`].
async fn lan_approval_gate<W>(
    accept: &LanAccept,
    peer: &PeerIdentity,
    device_name: &str,
    write_half: &mut W,
) -> LanGate
where
    W: tokio::io::AsyncWrite + Unpin,
{
    // A pinned device is already approved — straight to the version gate.
    if peer.decision == PinDecision::Known {
        return LanGate::Proceed;
    }

    // Unknown device: reserve a pending-approval hold. A full cap refuses `Busy`
    // so unapproved dialers can neither accumulate holds nor occupy a slot across
    // an unbounded human-decision window (analysis S1).
    let ticket = match accept.control.pending_approvals.begin() {
        Ok(ticket) => ticket,
        Err(_cap_reached) => {
            refuse_lan(write_half, device_name, LanRefusal::Busy).await;
            return LanGate::Refused;
        }
    };
    let request_id = ticket.request_id();

    // Assemble the approval request from the completed handshake + advertised
    // name. The network label is resolved off the async runtime (a rare
    // first-pairing path); `name_collision` is an informational hint only.
    let (network_label, network_id) = current_network_label(&accept.networks).await;
    let name_collision = accept
        .control
        .trusted_devices
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .name_collision(device_name);
    let request = ApprovalRequest {
        device_id: peer.device_id,
        cert_der: peer.cert_der.as_ref().to_vec(),
        device_name: device_name.to_owned(),
        network_label,
        network_id,
        name_collision,
    };

    // Tell the dialer it is waiting (FR-014, US2.5), then raise the prompt on the
    // owning machine's own local client(s) — never over a remote transport.
    if !send_lan_pending(write_half).await {
        // The dialer vanished; the ticket drops here, releasing the hold.
        return LanGate::Refused;
    }
    push_lan_approval_request(&accept.server, request_id, &request).await;

    // Hold with NO data until the owning user decides or the hold times out.
    match ticket.wait(APPROVAL_TIMEOUT).await {
        ApprovalOutcome::Approved => {
            persist_trusted_device(&accept.control, &request).await;
            // Decision-axis audit line, paired with the `lan: accepted …` line the
            // shared dispatch emits once a window is attached
            // (contracts/settings-and-config.md Audit log surface).
            info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: approved device={device_name} id={} network={}",
                short_device_id(&peer.device_id),
                request.network_label
            );
            LanGate::Proceed
        }
        ApprovalOutcome::Declined | ApprovalOutcome::TimedOut => {
            // Decision-axis audit line: an approval timeout collapses into the same
            // `Declined` outcome the data-model state and the wire refusal use, so it
            // is audited identically. The paired connection-axis line is the
            // `lan: refused … reason=declined` that `refuse_lan` emits next.
            info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: declined device={device_name} id={}",
                short_device_id(&peer.device_id)
            );
            refuse_lan(write_half, device_name, LanRefusal::Declined).await;
            LanGate::Refused
        }
    }
}

/// Read the mandatory `LanHello` preamble as the first frame after the mutual-TLS
/// handshake, bounded in size ([`REMOTE_PREAMBLE_MAX_BYTES`]) and time
/// ([`REMOTE_HANDSHAKE_TIMEOUT`]) exactly as the tailnet preamble. Returns the
/// peer's advertised display name + `remote_protocol_version`, or `None` for any
/// oversized / malformed / non-`LanHello` / timed-out / EOF first frame (all
/// close bare).
async fn read_lan_preamble<R>(reader: &mut R) -> Option<(String, u32)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let read = read_bounded_handshake(reader);
    match tokio::time::timeout(REMOTE_HANDSHAKE_TIMEOUT, read).await {
        Ok(Ok(ClientMessage::LanHello { device_name, remote_protocol_version })) => {
            Some((device_name, remote_protocol_version))
        }
        Ok(Ok(other)) => {
            debug!(?other, "LAN first frame was not LanHello; closing");
            None
        }
        Ok(Err(e)) => {
            debug!("LAN preamble read failed: {e}");
            None
        }
        Err(_) => {
            debug!("LAN preamble timed out; closing");
            None
        }
    }
}

/// Write `ServerMessage::LanApprovalPending` to the dialer (owning → connecting)
/// so it can show a "waiting for approval on <peer>" state before any window data
/// (FR-014). Returns whether the write landed; a dead link releases the hold.
async fn send_lan_pending<W>(write_half: &mut W) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match write_message(write_half, &ServerMessage::LanApprovalPending).await {
        Ok(()) => true,
        Err(e) => {
            debug!("failed to write LanApprovalPending: {e}");
            false
        }
    }
}

/// Write the terminal `ServerMessage::LanApprovalResult` to the dialer: `approved
/// = true` clears it to send `Hello`, otherwise `refusal` names the typed reason
/// and the caller closes. Returns whether the write landed.
async fn send_lan_result<W>(write_half: &mut W, approved: bool, refusal: Option<LanRefusal>) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let msg = ServerMessage::LanApprovalResult { approved, refusal };
    match write_message(write_half, &msg).await {
        Ok(()) => true,
        Err(e) => {
            debug!("failed to write LanApprovalResult: {e}");
            false
        }
    }
}

/// Refuse a LAN dialer: write the typed `LanApprovalResult` refusal and emit the
/// `lan: refused …` audit line (contracts/settings-and-config.md). The caller
/// closes the connection afterward.
async fn refuse_lan<W>(write_half: &mut W, device_name: &str, refusal: LanRefusal)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    send_lan_result(write_half, false, Some(refusal)).await;
    info!(
        target: REMOTE_AUDIT_TARGET,
        "lan: refused device={device_name} reason={}",
        audit_lan_refusal(refusal)
    );
}

/// Canonical audit reason token for a LAN refusal, mirroring the wire
/// [`LanRefusal`] taxonomy exactly (contracts/settings-and-config.md Audit log
/// surface) so the log and the wire never drift.
fn audit_lan_refusal(refusal: LanRefusal) -> &'static str {
    match refusal {
        LanRefusal::Declined => "declined",
        LanRefusal::NotTrustedNetwork => "not-trusted-network",
        LanRefusal::Disabled => "disabled",
        LanRefusal::IncompatibleVersion => "version",
        LanRefusal::Busy => "busy",
    }
}

/// Canonical audit reason token for a LAN dormancy transition, mirroring the
/// `lan: dormant reason=<network-untrusted|disabled>` taxonomy exactly
/// (contracts/settings-and-config.md Audit log surface). Deliberately distinct
/// from [`audit_lan_refusal`]'s `not-trusted-network` token — a refusal and a
/// dormancy are different events with different reason vocabularies, so they do
/// not share a token function and cannot silently drift.
fn audit_dormant_reason(reason: LanDormantReason) -> &'static str {
    match reason {
        LanDormantReason::Disabled => "disabled",
        LanDormantReason::NetworkUntrusted => "network-untrusted",
    }
}

/// A short, log-friendly rendering of a pinned LAN `device_id` (the first 8 bytes
/// as lowercase hex) for the `id=<short>` audit field — enough to disambiguate a
/// handful of devices without printing the full 256-bit id.
fn short_device_id(device_id: &DeviceId) -> String {
    fn hex_digit(nibble: u8) -> char {
        char::from(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 })
    }
    device_id
        .iter()
        .take(8)
        .flat_map(|&byte| [hex_digit(byte >> 4), hex_digit(byte & 0x0f)])
        .collect()
}

/// Resolve the trusted network a LAN request arrived on for the approval prompt
/// (label + record id). The netdev fingerprint read is blocking, so it runs off
/// the async runtime; a poisoned store or an unidentifiable network yields an
/// empty label (the prompt still shows the device + fingerprint words).
async fn current_network_label(networks: &SharedTrustedNetworks) -> (String, Option<String>) {
    let networks = Arc::clone(networks);
    let info = tokio::task::spawn_blocking(move || {
        networks.lock().ok().and_then(|store| store.current_trusted_network())
    })
    .await
    .ok()
    .flatten();
    match info {
        Some(info) => (info.label, Some(info.id)),
        None => (String::new(), None),
    }
}

/// Persist an approved LAN device as a `TrustedDevice` pin (off the async runtime
/// — the store write is blocking). A persistence failure is logged, not fatal:
/// this connection is already approved for its lifetime; the device simply needs
/// fresh approval next time.
async fn persist_trusted_device(control: &Arc<RemoteControl>, request: &ApprovalRequest) {
    let store = Arc::clone(&control.trusted_devices);
    let request = request.clone();
    match tokio::task::spawn_blocking(move || {
        store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).approve(&request)
    })
    .await
    {
        Ok(Ok(_info)) => {}
        Ok(Err(error)) => {
            warn!(%error, "failed to persist approved LAN device; it will need re-approval");
        }
        Err(join_error) => warn!(%join_error, "LAN trusted-device persist task panicked"),
    }
}

/// Push a `ServerMessage::LanApprovalRequest` to the owning machine's OWN local
/// client(s) — the GUI answers the prompt, never the remote TLS stream. Only
/// locally-controlled windows receive it (a remote peer must never see another
/// device's pending approval), so it is refused over any remote transport by
/// construction. The eventual `ClientMessage::LanApprovalDecision` correlates back
/// by `request_id`.
async fn push_lan_approval_request(
    server: &IpcServerState,
    request_id: u64,
    request: &ApprovalRequest,
) {
    let msg = ServerMessage::LanApprovalRequest {
        request_id,
        device_name: request.device_name.clone(),
        fingerprint_words: request.fingerprint_words(),
        network_label: request.network_label.clone(),
        name_collision: request.name_collision,
    };
    // Snapshot the local windows' writers under the canonical lock order
    // (connected_clients → window_controllers), then send with no lock held.
    let local_writers: Vec<SharedWriter> = {
        let clients = server.connected_clients.read().await;
        let controllers = server.window_controllers.read().await;
        clients
            .iter()
            .filter(|(window_id, _)| {
                matches!(controllers.get(window_id), Some(ControllerIdentity::Local))
            })
            .map(|(_, writer)| Arc::clone(writer))
            .collect()
    };
    if local_writers.is_empty() {
        debug!(request_id, "no local client to show the LAN approval prompt; awaiting timeout");
        return;
    }
    for writer in &local_writers {
        send_message(writer, &msg).await;
    }
}

/// Enable TCP keepalive on an accepted remote stream so a peer that vanishes
/// without a FIN/RST (laptop sleep, Wi-Fi roam) is eventually torn down at the OS
/// layer, freeing its authorized-connection slot. Sets `SO_KEEPALIVE` plus tuned
/// idle/interval timers ([`REMOTE_KEEPALIVE_IDLE`]/[`REMOTE_KEEPALIVE_INTERVAL`])
/// via `socket2`, which maps them to the right per-platform sockopts
/// (`TCP_KEEPIDLE` on Linux, `TCP_KEEPALIVE` on macOS) so a dead TCP path is
/// detected in a few minutes rather than waiting out the application idle-read
/// timeout ([`REMOTE_IDLE_READ_TIMEOUT`]), which remains the backstop for a
/// TCP-alive but app-silent peer. Best-effort: a failure only forgoes the probe.
fn enable_tcp_keepalive(stream: &TcpStream, peer_addr: SocketAddr) {
    let params = socket2::TcpKeepalive::new()
        .with_time(REMOTE_KEEPALIVE_IDLE)
        .with_interval(REMOTE_KEEPALIVE_INTERVAL);
    let sock = socket2::SockRef::from(stream);
    if let Err(e) = sock.set_keepalive(true) {
        debug!(%peer_addr, "failed to enable SO_KEEPALIVE on remote connection: {e}");
        return;
    }
    if let Err(e) = sock.set_tcp_keepalive(&params) {
        debug!(%peer_addr, "failed to tune TCP keepalive on remote connection: {e}");
    }
}

/// Handle one accepted remote (TCP) connection per contracts/remote-protocol.md:
/// read the `RemoteHandshake` preamble FIRST (bounded), resolve + authorize the
/// tailnet identity, gate the protocol version and the connection cap, ALWAYS
/// answer a typed `RemoteHandshakeReply`, and — only on acceptance — hand the
/// stream to the shared per-connection dispatch. A malformed or non-preamble
/// first frame closes bare (nothing protocol-aware to answer).
async fn handle_remote_client(
    stream: TcpStream,
    peer_addr: SocketAddr,
    server: IpcServerState,
    control: Arc<RemoteControl>,
) {
    let (mut reader, mut write_half) = tokio::io::split(stream);

    let Some(client_version) = read_remote_preamble(&mut reader).await else {
        // Malformed / non-`RemoteHandshake` first frame, timeout, or EOF: bare
        // close, no reply (FR-003 — nothing protocol-aware to answer).
        return;
    };

    match authorize_remote(&control, peer_addr, client_version).await {
        Ok((identity, permit)) => {
            // Reserve the sever-channel registration — which re-checks the disable
            // flag under the same lock `sever_all_connections` drains under — BEFORE
            // answering the handshake. This closes the disable race: a connection
            // that raced a live disable between authorization and here is refused
            // with a typed `Disabled` reply instead of being told `accepted` and
            // then silently dropped (FR-016; contracts/remote-protocol.md Disable
            // semantics). Registered before serving so even a pre-Hello connection
            // is signalable; deregistered once the connection is fully done.
            let (sever_tx, sever_rx) = tokio::sync::oneshot::channel();
            let Some(conn_id) =
                control.register_connection(Transport::Tailnet, sever_tx, None).await
            else {
                send_handshake_reply(&mut write_half, Some(RemoteRefusal::Disabled)).await;
                info!(
                    target: REMOTE_AUDIT_TARGET,
                    "remote: refused peer={} reason={}",
                    identity.node_name,
                    audit_reason(RemoteRefusal::Disabled)
                );
                // The cap permit drops with this scope.
                return;
            };
            send_handshake_reply(&mut write_half, None).await;
            // Feature 013 (T029, research D5): interpose a bounded per-connection
            // output queue between the fan-out hot path and this (possibly slow)
            // tailnet link. The drain task owns the TCP write half; the
            // `SharedWriter` only enqueues, so a stalled remote consumer can never
            // block the server's authoritative `Term` or the other clients. Local
            // Unix-socket connections write inline (see `handle_client`).
            let (sink, drain_task) = spawn_remote_output(write_half, server.clone());
            let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::Remote(sink.clone())));
            let ctx = RemoteContext {
                node_name: identity.node_name,
                login_name: identity.login_name,
                audit: RemoteAudit::Tailnet,
                sever: sever_rx,
            };
            serve_connection(reader, writer, server, Some(ctx)).await;
            // Flush any final frame (e.g. the sever `RemoteDisconnect`) and stop the
            // drain task before releasing the connection slot.
            shutdown_remote_output(&sink, drain_task).await;
            control.deregister_connection(Transport::Tailnet, conn_id).await;
            // Release the connection-cap slot once the connection is fully done.
            drop(permit);
        }
        Err(reject) => {
            send_handshake_reply(&mut write_half, Some(reject.refusal)).await;
            let detail = if reject.tagged { " detail=tagged" } else { "" };
            info!(
                target: REMOTE_AUDIT_TARGET,
                "remote: refused peer={} reason={}{}",
                reject.peer_label,
                audit_reason(reject.refusal),
                detail
            );
        }
    }
}

/// A typed refusal decided by [`authorize_remote`], with the audit peer label
/// and the tagged-node qualifier.
struct RemoteReject {
    refusal: RemoteRefusal,
    /// Peer label for the audit line: the tailnet node name when known,
    /// otherwise the connecting address.
    peer_label: String,
    /// Whether an `Unauthorized` refusal was specifically a tagged/identity-less
    /// node (audit `detail=tagged`).
    tagged: bool,
}

/// Resolve and gate an accepted remote connection after its preamble is read:
/// disable-race → identity/authorization (fail closed) → exact version match →
/// connection cap. Returns the authorized identity plus the held cap permit, or
/// a typed [`RemoteReject`].
async fn authorize_remote(
    control: &Arc<RemoteControl>,
    peer_addr: SocketAddr,
    client_version: u32,
) -> Result<(crate::tailnet::TailnetIdentity, tokio::sync::OwnedSemaphorePermit), RemoteReject> {
    // Disable race: `remote.enabled` flipped off between accept and here.
    if !control.tailnet.enabled.load(Ordering::SeqCst) {
        return Err(RemoteReject {
            refusal: RemoteRefusal::Disabled,
            peer_label: peer_addr.ip().to_string(),
            tagged: false,
        });
    }

    // Identity + same-account authorization; any LocalAPI failure fails closed.
    let identity = match crate::tailnet::authorize_peer(peer_addr).await {
        Ok(identity) => identity,
        Err(crate::tailnet::PeerAuthError::Unauthorized { identity, tagged }) => {
            return Err(RemoteReject {
                refusal: RemoteRefusal::Unauthorized,
                peer_label: identity.node_name,
                tagged,
            });
        }
        Err(crate::tailnet::PeerAuthError::IdentityUnavailable(e)) => {
            debug!(%peer_addr, "remote identity unavailable: {e}");
            return Err(RemoteReject {
                refusal: RemoteRefusal::IdentityUnavailable,
                peer_label: peer_addr.ip().to_string(),
                tagged: false,
            });
        }
    };

    // Exact protocol-version gate (v1 policy, research D3).
    if client_version != REMOTE_PROTOCOL_VERSION {
        return Err(RemoteReject {
            refusal: RemoteRefusal::IncompatibleVersion,
            peer_label: identity.node_name,
            tagged: false,
        });
    }

    // Connection cap: reserve a slot, held for the connection's life.
    match Arc::clone(&control.tailnet.conn_limit).try_acquire_owned() {
        Ok(permit) => Ok((identity, permit)),
        Err(_) => Err(RemoteReject {
            refusal: RemoteRefusal::Busy,
            peer_label: identity.node_name,
            tagged: false,
        }),
    }
}

/// Read the mandatory `RemoteHandshake` preamble as the first frame, bounded in
/// both size ([`REMOTE_PREAMBLE_MAX_BYTES`], far below the shared 64 MiB frame
/// budget so a forged length prefix cannot force a large pre-auth allocation) and
/// time ([`REMOTE_HANDSHAKE_TIMEOUT`]). Returns the dialer's
/// `remote_protocol_version`, or `None` for any oversized / malformed /
/// non-preamble / timed-out / EOF first frame (all of which close bare).
async fn read_remote_preamble<R>(reader: &mut R) -> Option<u32>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let read = read_bounded_handshake(reader);
    match tokio::time::timeout(REMOTE_HANDSHAKE_TIMEOUT, read).await {
        Ok(Ok(ClientMessage::RemoteHandshake { remote_protocol_version, .. })) => {
            Some(remote_protocol_version)
        }
        Ok(Ok(other)) => {
            debug!(?other, "remote first frame was not RemoteHandshake; closing");
            None
        }
        Ok(Err(e)) => {
            debug!("remote handshake read failed: {e}");
            None
        }
        Err(_) => {
            debug!("remote handshake preamble timed out; closing");
            None
        }
    }
}

/// Read one length-prefixed `MessagePack` `ClientMessage` frame under a small
/// pre-auth size cap ([`REMOTE_PREAMBLE_MAX_BYTES`]) — deliberately NOT the shared
/// 64 MiB [`read_message`] budget, so an unauthenticated dialer's forged length
/// prefix cannot make the server allocate a large buffer before identity,
/// authorization, and version are checked. Same framing/decoding as
/// [`read_message`], just with the tighter bound.
async fn read_bounded_handshake<R>(reader: &mut R) -> Result<ClientMessage, ScribeError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let len = reader.read_u32().await.map_err(|e| ScribeError::Io { source: e })?;
    if len > REMOTE_PREAMBLE_MAX_BYTES {
        return Err(ScribeError::ProtocolError {
            reason: format!("remote preamble size {len} exceeds cap {REMOTE_PREAMBLE_MAX_BYTES}"),
        });
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await.map_err(|e| ScribeError::Io { source: e })?;
    rmp_serde::from_slice(&buf).map_err(Into::into)
}

/// Write the mandatory `RemoteHandshakeReply` (accepted when `refusal` is `None`,
/// otherwise the typed refusal). Always carries the server's remote-protocol and
/// Scribe versions for the client's mismatch copy.
async fn send_handshake_reply<W>(writer: &mut W, refusal: Option<RemoteRefusal>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let reply = ServerMessage::RemoteHandshakeReply {
        accepted: refusal.is_none(),
        refusal,
        server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        server_scribe_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if let Err(e) = write_message(writer, &reply).await {
        debug!("failed to write remote handshake reply: {e}");
    }
}

/// Best-effort, time-bounded final `ServerMessage::RemoteDisconnect` sent to a
/// remote client the server is severing on disable (T023). The socket closes
/// immediately afterward whether or not this frame lands; a wedged link is capped
/// by [`REMOTE_SEVER_NOTICE_TIMEOUT`] so the 2-second disable budget holds. A lost
/// notice is contract-safe: the client falls back to the reconnect path and the
/// vanished listener yields the combined connection-failure copy.
async fn send_remote_disconnect(writer: &SharedWriter, reason: RemoteRefusal) {
    let msg = ServerMessage::RemoteDisconnect { reason };
    if tokio::time::timeout(REMOTE_SEVER_NOTICE_TIMEOUT, try_send_message(writer, &msg))
        .await
        .is_err()
    {
        debug!("best-effort RemoteDisconnect send timed out during sever");
    }
}

/// Canonical audit reason token for a refusal, mirroring the wire `RemoteRefusal`
/// taxonomy exactly (contracts/settings-and-config.md Audit log surface).
fn audit_reason(refusal: RemoteRefusal) -> &'static str {
    match refusal {
        RemoteRefusal::Disabled => "disabled",
        RemoteRefusal::Unauthorized => "unauthorized",
        RemoteRefusal::IdentityUnavailable => "identity-unavailable",
        RemoteRefusal::IncompatibleVersion => "version",
        RemoteRefusal::Busy => "busy",
    }
}

/// Acquire the server socket with singleton enforcement.
///
/// In normal mode, uses an advisory flock on `server.lock` to serialise
/// the bind-or-connect sequence.  If another server already holds the
/// socket, returns `IpcError` ("already running").  In upgrade mode the
/// lock and liveness check are skipped — the handoff protocol coordinates
/// the two servers, and the old server still holds the lock.
///
/// Returns the lock file guard (must be kept alive) and the bound listener.
pub fn acquire_server_socket(
    socket_path: &Path,
    upgrade_mode: bool,
) -> Result<(Option<nix::fcntl::Flock<std::fs::File>>, UnixListener), ScribeError> {
    // Ensure the parent directory exists with 0700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    if upgrade_mode {
        // Upgrade mode: unconditionally replace the socket.  The handoff
        // protocol has already coordinated with the old server.
        drop(std::fs::remove_file(socket_path));
        return Ok((None, try_bind(socket_path)?));
    }

    // Normal mode: acquire flock then bind-or-connect.
    let lock_path = scribe_common::socket::server_lock_path();
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| ScribeError::Io { source: e })?;

    let lock_file = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, _)| ScribeError::IpcError {
            reason: "another scribe-server is already running (lock held)".into(),
        })?;

    // Try to bind the socket.  If it fails with EADDRINUSE the path
    // already exists; any other error is a real failure.
    match UnixListener::bind(socket_path) {
        Ok(listener) => {
            set_socket_permissions(socket_path);
            Ok((Some(lock_file), listener))
        }
        Err(bind_err) if bind_err.kind() == std::io::ErrorKind::AddrInUse => {
            // Socket file exists — check if another server is alive.
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return Err(ScribeError::IpcError {
                    reason: "another scribe-server is already running".into(),
                });
            }
            // Stale socket from a crashed server — remove and retry.
            info!("removing stale server socket");
            drop(std::fs::remove_file(socket_path));
            Ok((Some(lock_file), try_bind(socket_path)?))
        }
        Err(bind_err) => Err(ScribeError::Io { source: bind_err }),
    }
}

/// Bind the Unix socket and set file permissions to 0o600 (defense-in-depth).
fn try_bind(socket_path: &Path) -> Result<UnixListener, ScribeError> {
    let listener = UnixListener::bind(socket_path).map_err(|e| ScribeError::Io { source: e })?;
    set_socket_permissions(socket_path);
    Ok(listener)
}

/// Set socket file permissions to owner-only (defense-in-depth alongside
/// `SO_PEERCRED` UID verification and the 0o700 parent directory).
fn set_socket_permissions(socket_path: &Path) {
    if let Err(e) = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)) {
        warn!(?socket_path, "failed to set socket permissions: {e}");
    }
}

/// Verify the connecting peer has the same UID as this server process.
fn verify_peer_uid(stream: &tokio::net::UnixStream) -> bool {
    let cred: UCred = match stream.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to get peer credentials, rejecting: {e}");
            return false;
        }
    };

    let expected = current_uid();
    if cred.uid() != expected {
        warn!(peer_uid = cred.uid(), expected, "rejected connection from different UID");
        return false;
    }
    true
}

/// Per-client connection handler for the local Unix socket. Splits the stream
/// and drives the shared [`serve_connection`] core (`Hello`/`Welcome` handshake
/// then message dispatch). Local connections carry no remote context.
async fn handle_client(stream: tokio::net::UnixStream, server: IpcServerState) {
    let (reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::Local(Box::new(writer))));
    serve_connection(reader, writer, server, None).await;
}

/// Drive the post-handshake per-connection protocol over any framed stream —
/// shared verbatim by the local Unix-socket path and the feature-013 remote TCP
/// path. `remote` is `Some` only for accepted remote connections: it gates the
/// transient no-Hello actions off (they are local-socket only) and carries the
/// tailnet identity for the accepted/disconnect audit lines. Local connections
/// (`None`) behave exactly as before.
async fn serve_connection<R>(
    mut reader: R,
    writer: SharedWriter,
    server: IpcServerState,
    remote: Option<RemoteContext>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    // Track which sessions this client has attached to, for detach on disconnect.
    let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

    // Feature 013: split the remote context into the display identity (kept for
    // the accepted/disconnect audit lines) and the sever signal (moved into the
    // read paths so a disable can drop this connection out of its loop). Local
    // Unix-socket connections carry neither.
    let (remote_identity, mut sever_rx) = match remote {
        Some(ctx) => (
            Some(RemoteIdentity {
                node_name: ctx.node_name,
                login_name: ctx.login_name,
                audit: ctx.audit,
            }),
            Some(ctx.sever),
        ),
        None => (None, None),
    };

    // The connection's controller identity — `Local` for the Unix socket,
    // `Remote { device, account }` for an authenticated tailnet peer. Threaded
    // into the claim path so a takeover can name the new controller and the window
    // list can surface who holds each window (FR-007, FR-009b).
    let controller = remote_identity.as_ref().map_or(ControllerIdentity::Local, |id| {
        ControllerIdentity::Remote {
            device_name: id.node_name.clone(),
            login_name: id.login_name.clone(),
        }
    });

    let conn = ConnState { server: &server, writer: &writer, attached_ids: &attached_ids };

    let Some(window_id) =
        establish_client_window(&mut reader, conn, &controller, sever_rx.as_mut()).await
    else {
        // A bare pre-Hello close, or remote access was disabled before Hello
        // arrived (T023). No window was claimed, so just close.
        if remote_identity.is_some() {
            debug!("remote connection closed before establishing a window");
        }
        return;
    };

    if let Some(id) = &remote_identity {
        match &id.audit {
            // Feature 013 tailnet line, preserved byte-for-byte.
            RemoteAudit::Tailnet => info!(
                target: REMOTE_AUDIT_TARGET,
                "remote: accepted peer={} user={} window={window_id}", id.node_name, id.login_name
            ),
            // Feature 014 LAN line (contracts/settings-and-config.md).
            RemoteAudit::Lan { device_id_short } => info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: accepted device={} id={device_id_short} window={window_id}", id.node_name
            ),
        }
    }

    let exit = run_client_message_loop(&mut reader, window_id, conn, sever_rx).await;
    finish_served_connection(exit, window_id, conn, remote_identity.as_ref()).await;
}

/// Tear a served connection down after its message loop returns: on a sever, send
/// the best-effort final `RemoteDisconnect` (T023); always run the detach cleanup
/// (window ownership released, owning sessions untouched); then, for a remote
/// connection, log the per-connection `disconnect` audit line — but only for a
/// genuine peer disconnect, since a sever is covered by the single bulk `severed`
/// line emitted on disable.
async fn finish_served_connection(
    exit: LoopExit,
    window_id: WindowId,
    conn: ConnState<'_>,
    remote_identity: Option<&RemoteIdentity>,
) {
    if matches!(exit, LoopExit::Severed) {
        send_remote_disconnect(conn.writer, RemoteRefusal::Disabled).await;
    }

    detach_client_window(window_id, conn.server, conn.attached_ids, conn.writer).await;

    if let (Some(id), LoopExit::Disconnected) = (remote_identity, exit) {
        match &id.audit {
            // Feature 013 tailnet line, preserved byte-for-byte.
            RemoteAudit::Tailnet => info!(
                target: REMOTE_AUDIT_TARGET,
                "remote: disconnect peer={} window={window_id}", id.node_name
            ),
            // Feature 014 LAN line (contracts/settings-and-config.md).
            RemoteAudit::Lan { .. } => info!(
                target: REMOTE_AUDIT_TARGET,
                "lan: disconnect device={} window={window_id}", id.node_name
            ),
        }
    }
}

async fn establish_client_window<R>(
    reader: &mut R,
    conn: ConnState<'_>,
    controller: &ControllerIdentity,
    mut sever_rx: Option<&mut tokio::sync::oneshot::Receiver<()>>,
) -> Option<WindowId>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let is_remote = matches!(controller, ControllerIdentity::Remote { .. });
    loop {
        // Race each pre-Hello read against the sever signal so a disable (T023)
        // closes a remote connection even before it claims a window; the read also
        // carries the remote idle-read timeout so an abandoned pre-Hello dialer
        // frees its slot. Local connections pass no sever channel and never time
        // out, so this degenerates to today's bare read.
        let first = tokio::select! {
            biased;
            () = await_sever(sever_rx.as_deref_mut()) => {
                debug!("remote access disabled before Hello; closing");
                return None;
            }
            read = read_client_frame(reader, is_remote) => read,
        };
        let Some(first) = first else {
            debug!("remote connection idle before Hello; closing");
            return None;
        };
        match first {
            Ok(ClientMessage::Hello { window_id, clipboard_gating, takeover }) => {
                let claim = HelloClaim {
                    requested_window_id: window_id,
                    clipboard_gating,
                    takeover,
                    controller,
                };
                return Some(handle_client_hello(claim, conn.server, conn.writer).await);
            }
            // Feature 013 (fix 5): the picker's window-probe. An ALREADY-authorized
            // remote connection may enumerate this machine's windows read-only
            // BEFORE `Hello`; reply with `WindowList` and keep reading (the probe
            // then closes, or — tolerated — a `Hello` follows on the same link). No
            // window is registered by a bare `ListWindows`. Every OTHER non-Hello
            // first frame still closes (transient no-Hello actions stay local-only).
            Ok(ClientMessage::ListWindows) if is_remote => {
                handle_list_windows(
                    &conn.server.connected_clients,
                    &conn.server.window_controllers,
                    &conn.server.workspace_manager,
                    conn.writer,
                )
                .await;
                // Fall through to the next loop iteration to read the next frame.
            }
            // Remote (TCP) connections: any other first frame after the accepted
            // handshake MUST be `Hello`. Transient no-Hello actions (update checks,
            // hook events, `ListRemotePeers`) and legacy no-Hello claims are
            // local-socket only per contracts/remote-protocol.md, so refuse by
            // closing.
            Ok(other) if is_remote => {
                debug!(
                    ?other,
                    "remote connection sent an unexpected non-Hello first frame; closing"
                );
                return None;
            }
            Ok(msg) => {
                return establish_local_first_frame(
                    msg,
                    conn.server,
                    conn.writer,
                    conn.attached_ids,
                )
                .await;
            }
            Err(ScribeError::Io { .. }) => {
                debug!("client disconnected before Hello");
                return None;
            }
            Err(e) => {
                warn!("failed to read Hello message: {e}");
                return None;
            }
        }
    }
}

/// Read the next client frame, applying the remote idle-read timeout
/// ([`REMOTE_IDLE_READ_TIMEOUT`]) ONLY for remote (TCP) connections so an
/// abandoned peer's scarce connection slot is reclaimed. Returns `None` when the
/// idle timeout expires (the caller treats that as a disconnect); local
/// Unix-socket connections (`is_remote == false`) keep today's untimed read and
/// always return `Some`.
async fn read_client_frame<R>(
    reader: &mut R,
    is_remote: bool,
) -> Option<Result<ClientMessage, ScribeError>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let read = read_message::<ClientMessage, _>(reader);
    if is_remote {
        tokio::time::timeout(REMOTE_IDLE_READ_TIMEOUT, read).await.ok()
    } else {
        Some(read.await)
    }
}

/// Handle a non-`Hello` first frame on the LOCAL Unix socket: the transient
/// no-Hello actions (update checks, hook events) that never register a window,
/// or a legacy client whose first message is not a `Hello`. All are local-socket
/// only — the remote path refuses them in [`establish_client_window`]. Split out
/// so the establish path stays under the cognitive-complexity budget.
async fn establish_local_first_frame(
    msg: ClientMessage,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> Option<WindowId> {
    match msg {
        ClientMessage::CheckForUpdates => {
            // Transient action: the caller (e.g. the standalone settings
            // window) does not want a registered window. Run the check, send
            // back a single result, and let the connection close without ever
            // entering `connected_clients`.
            handle_transient_check_for_updates(server, writer).await;
            None
        }
        ClientMessage::ListReleases => {
            // Same transient-action shape as `CheckForUpdates`: settings
            // window opens a fresh socket, sends `ListReleases`, reads one
            // `ReleaseList` reply, closes. No window registration required.
            handle_transient_list_releases(server, writer).await;
            None
        }
        ClientMessage::TriggerUpdate => {
            // Transient install kick-off from the standalone settings
            // window. The updater's single-slot trigger channel collapses
            // duplicate requests, so it is safe even if an in-client
            // overlay is already mid-install. No reply frame.
            handle_transient_trigger_update(server);
            None
        }
        ClientMessage::HookEvent(event) => {
            // Transient one-shot: scribe-hook-helper sends one HookEvent,
            // server dispatches, connection closes. No Welcome, no reply.
            // See specs/003-ai-hook-channel/contracts/wire-protocol.md.
            hook_ingress::handle(server, event).await;
            None
        }
        ClientMessage::ListRemotePeers => {
            // Feature 013 transient local-only action (contracts/remote-protocol
            // Local helper messages): enumerate this machine's own same-account
            // tailnet peers for the connect picker, reply once, close. Local-
            // socket only by construction — remote (TCP) connections never reach
            // here because a non-Hello first frame is refused in
            // `establish_client_window`, satisfying "refused over TCP" (a remote
            // peer has no business enumerating a third machine's tailnet view).
            handle_transient_list_remote_peers(writer).await;
            None
        }
        ClientMessage::ListLanPeers => {
            // Feature 014 T013 transient local-only action (contracts/lan-protocol
            // Local-only helper messages): enumerate this machine's own mDNS-
            // discovered LAN peers for the connect picker, reply once, close.
            // Local-socket only by the same construction as `ListRemotePeers` — a
            // non-Hello first frame is refused over any remote transport in
            // `establish_client_window`, so a remote peer cannot read a third
            // machine's LAN discovery view.
            handle_transient_list_lan_peers(server, writer).await;
            None
        }
        ClientMessage::GetRemoteEnv => {
            // Feature 013 transient local-only action: report this machine's
            // signed-in tailnet account + whether Tailscale is detected for the
            // Settings → Remote section, reply once, close. Local-socket only by
            // the same construction as `ListRemotePeers` — a non-Hello first
            // frame is refused over TCP in `establish_client_window`, so a remote
            // peer cannot read a third machine's tailnet identity.
            handle_transient_get_remote_env(writer).await;
            None
        }
        ClientMessage::GetLanEnv => {
            // Feature 014 transient local-only action (the LAN analog of
            // `GetRemoteEnv`): report this machine's OWN device fingerprint and
            // whether its current network can be trusted for the Settings → Remote
            // "Local network" section, reply once, close. Read-only — it never
            // generates the device identity (that happens on first LAN enable). Local-
            // socket only by the same construction as `ListLanPeers` — a non-Hello
            // first frame is refused over any remote transport in
            // `establish_client_window`, so a remote peer cannot read a third
            // machine's own identity fingerprint.
            handle_transient_get_lan_env(writer).await;
            None
        }
        ClientMessage::ListTrustedNetworks => {
            // Feature 014 T021 transient local-only action (contracts/lan-protocol
            // Local-only helper messages): report this machine's trusted networks
            // and whether its current network is trusted, reply once, close. Local-
            // socket only by the same construction as `ListLanPeers` — a non-Hello
            // first frame is refused over any remote transport in
            // `establish_client_window`.
            handle_transient_list_trusted_networks(server, writer).await;
            None
        }
        ClientMessage::AddCurrentNetworkTrusted => {
            // Feature 014 T021 transient local-only mutation: trust the network this
            // machine is currently on, then poke the supervisor so the enabled LAN
            // transport can activate on the now-trusted network. Fire-and-forget —
            // no reply frame (the settings UI re-queries the list/env afterward).
            // Local-socket only by the same construction as `ListLanPeers`.
            handle_transient_add_current_network(server).await;
            None
        }
        ClientMessage::RemoveTrustedNetwork { id } => {
            // Feature 014 T021 transient local-only mutation: remove a trusted
            // network by id, then poke the supervisor so removing the CURRENT
            // network takes the LAN surface dormant promptly (FR-018). Fire-and-
            // forget — no reply frame. Local-socket only by the same construction
            // as `ListLanPeers`.
            handle_transient_remove_trusted_network(server, id).await;
            None
        }
        ClientMessage::ListTrustedDevices => {
            // Feature 014 T027 transient local-only action (contracts/lan-protocol
            // Local-only helper messages): list this machine's approved LAN devices
            // for the Settings → Remote "Local network" section, reply once, close.
            // Local-socket only by the same construction as `ListLanPeers` — a
            // non-Hello first frame is refused over any remote transport in
            // `establish_client_window`, so a remote peer cannot read a third
            // machine's trusted-device store.
            handle_transient_list_trusted_devices(server, writer).await;
            None
        }
        ClientMessage::RevokeTrustedDevice { device_id } => {
            // Feature 014 T027 transient local-only mutation: revoke a trusted LAN
            // device by its hex `device_id`, removing the pin and severing only that
            // device's live LAN connection so it must re-approve next time (FR-010).
            // Fire-and-forget — no reply frame (the settings UI refreshes the list
            // afterward). Local-socket only by the same construction as
            // `ListLanPeers`.
            handle_transient_revoke_trusted_device(server, device_id).await;
            None
        }
        other => Some(handle_legacy_client(other, server, writer, attached_ids).await),
    }
}

async fn handle_transient_check_for_updates(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested manual update check");
    let state = server.updater_handle.request_check().await;
    send_message(writer, &ServerMessage::UpdateCheckResult { state }).await;
}

fn handle_transient_trigger_update(server: &IpcServerState) {
    info!("transient client triggered update");
    server.updater_handle.trigger();
}

async fn handle_transient_list_releases(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested release list");
    let state =
        crate::releases::handle_list_releases(&server.release_catalog, &server.release_fetcher)
            .await;
    send_message(writer, &ServerMessage::ReleaseList { state }).await;
}

/// Feature 013: serve a transient `ListRemotePeers` — this machine's own
/// same-account tailnet peers for the connect picker, resolved from `LocalAPI`
/// status. Any `LocalAPI` failure yields an empty list (the picker falls back to
/// manual host entry); the server never blocks local serving on tailscaled.
/// Offline peers are kept with `online = false` so the picker can grey them.
async fn handle_transient_list_remote_peers(writer: &SharedWriter) {
    info!("transient client requested remote peer list");
    let peers = match crate::tailnet::fetch_status().await {
        Ok(status) => status
            .peers
            .into_iter()
            .filter(|peer| peer.same_account)
            .map(|peer| RemotePeerInfo {
                name: peer.name,
                addr: peer.addr.to_string(),
                online: peer.online,
            })
            .collect(),
        Err(e) => {
            debug!("remote peer list unavailable: {e}");
            Vec::new()
        }
    };
    send_message(writer, &ServerMessage::RemotePeerList { peers }).await;
}

/// Feature 014 T013: serve a transient `ListLanPeers` — this machine's own
/// `mDNS`-discovered LAN peers for the connect picker, read from the supervisor's
/// live discovery view published on the shared [`RemoteControl`]. Empty while the
/// LAN transport is dormant or off (nothing is being browsed), so the picker
/// falls back to manual `host:port` entry — mirroring the tailnet
/// [`handle_transient_list_remote_peers`] empty-on-unavailable behavior. Never
/// blocks local serving on discovery.
async fn handle_transient_list_lan_peers(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested LAN peer list");
    let peers = server.remote_control.lan_peers();
    send_message(writer, &ServerMessage::LanPeerList { peers }).await;
}

/// Feature 014 T021: serve a transient `ListTrustedNetworks` — this machine's
/// trusted networks plus whether its CURRENT network is trusted, read from the
/// shared trusted-networks store on the [`RemoteControl`] (the same store the
/// supervisor's `apply_lan` gate and network-change watcher read). Local-socket
/// only by construction — a non-`Hello` first frame is refused over any remote
/// transport in `establish_client_window`, so a remote peer cannot read a third
/// machine's trust state. Replies once and closes.
async fn handle_transient_list_trusted_networks(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested trusted network list");
    let (networks, current_trusted) = server.remote_control.trusted_networks_snapshot().await;
    send_message(writer, &ServerMessage::TrustedNetworkList { networks, current_trusted }).await;
}

/// Feature 014 T021: serve a transient `AddCurrentNetworkTrusted` — trust the
/// network this machine is currently on and poke the supervisor so the enabled
/// LAN transport can activate on the now-trusted network. Fire-and-forget: no
/// reply frame (the settings UI re-issues the list/env queries afterward); a
/// failure (an unidentifiable network) is logged server-side and pre-empted by
/// the disabled "Add current network" control. Local-socket only by construction.
async fn handle_transient_add_current_network(server: &IpcServerState) {
    info!("transient client requested to trust the current network");
    server.remote_control.add_current_trusted_network().await;
}

/// Feature 014 T021: serve a transient `RemoveTrustedNetwork` — remove a trusted
/// network by id and poke the supervisor so removing the CURRENT network takes
/// the LAN surface dormant promptly (tear the listener down, `mDNS` goodbye, sever
/// only LAN connections; FR-018). Fire-and-forget: no reply frame (the settings
/// UI refreshes the list afterward). Local-socket only by construction.
async fn handle_transient_remove_trusted_network(server: &IpcServerState, id: String) {
    info!("transient client requested to remove a trusted network");
    server.remote_control.remove_trusted_network(id).await;
}

/// Feature 014 T027: serve a transient `ListTrustedDevices` — this machine's
/// approved LAN devices for the Settings "Local network" trusted-devices list,
/// read from the shared trusted-device pin store on the [`RemoteControl`] (the
/// same store the accept path's approval gate writes and the revoke handler
/// removes from). Local-socket only by construction — a non-`Hello` first frame
/// is refused over any remote transport in `establish_client_window`, so a remote
/// peer cannot read a third machine's trust state. Replies once and closes.
async fn handle_transient_list_trusted_devices(server: &IpcServerState, writer: &SharedWriter) {
    info!("transient client requested trusted device list");
    let devices = server.remote_control.list_trusted_devices();
    send_message(writer, &ServerMessage::TrustedDeviceList { devices }).await;
}

/// Feature 014 T027: serve a transient `RevokeTrustedDevice` — remove the pin for
/// a trusted LAN device and sever ONLY that device's live LAN connection via the
/// T009 `device_id -> connection-id` index, so it loses control at once and must
/// re-approve on its next attempt (FR-010, SC-006). Fire-and-forget: no reply
/// frame (the settings UI refreshes the list afterward). Local-socket only by
/// construction.
async fn handle_transient_revoke_trusted_device(server: &IpcServerState, device_id: String) {
    info!("transient client requested to revoke a trusted device");
    server.remote_control.revoke_trusted_device(device_id).await;
}

/// Feature 013: serve a transient `GetRemoteEnv` — this machine's signed-in
/// tailnet account name and whether Tailscale is detected at all, for the
/// Settings → Remote section (UX-003). Resolved from `LocalAPI` status. Any
/// `LocalAPI` failure fails closed to `{ account: None, tailscale_detected:
/// false }` (FR-015), which drives the passive "Tailscale not detected" notice;
/// the server never blocks local serving on tailscaled. An empty login name (a
/// signed-in node should always carry one) is normalised to `None` so the
/// statement keeps its generic account placeholder instead of rendering a blank.
async fn handle_transient_get_remote_env(writer: &SharedWriter) {
    info!("transient client requested remote env");
    let (account, tailscale_detected) = match crate::tailnet::fetch_status().await {
        Ok(status) => {
            let login = status.self_identity.login_name;
            ((!login.is_empty()).then_some(login), true)
        }
        Err(e) => {
            debug!("remote env unavailable: {e}");
            (None, false)
        }
    };
    send_message(writer, &ServerMessage::RemoteEnv { account, tailscale_detected }).await;
}

/// Feature 014: serve a transient `GetLanEnv` — this machine's OWN LAN identity
/// fingerprint (Device ID hex + words, both `None` until the identity is
/// generated on first LAN enable) plus whether the current network can be
/// fingerprinted as a trusted network, for the Settings → Remote "Local network"
/// section (FR-006, contracts/settings-and-config.md). READ-ONLY: it never mints
/// the device identity — merely opening Settings must not create a key. Any local
/// error (keyring unavailable, unidentifiable network) fails closed to identity
/// `None` / not-addable so the settings window always renders one shape and never
/// blocks. Local-socket only by the same construction as `GetRemoteEnv` — a
/// non-`Hello` first frame is refused over any remote transport in
/// `establish_client_window`, so a remote peer cannot read a third machine's own
/// identity fingerprint. Replies once and closes.
async fn handle_transient_get_lan_env(writer: &SharedWriter) {
    info!("transient client requested LAN env");
    let (device_id_hex, fingerprint_words) = match crate::lan::identity::own_fingerprint().await {
        Ok(Some(fingerprint)) => {
            (Some(fingerprint.device_id_hex), Some(fingerprint.fingerprint_words))
        }
        Ok(None) => (None, None),
        Err(error) => {
            debug!(%error, "own LAN fingerprint unavailable; reporting none");
            (None, None)
        }
    };
    // The `netdev` interface read is blocking, so resolve current-network
    // addability off the async runtime. An unidentifiable network yields a short
    // user-facing reason for the disabled "Add current network" control.
    let (current_network_addable, current_network_reason) =
        match tokio::task::spawn_blocking(network::current_network_fingerprint).await {
            Ok(Ok(_fingerprint)) => (true, None),
            Ok(Err(error)) => (false, Some(lan_network_addable_reason(error))),
            Err(join_error) => {
                debug!(%join_error, "LAN network-fingerprint task panicked; reporting not addable");
                (false, None)
            }
        };
    send_message(
        writer,
        &ServerMessage::LanEnv {
            device_id_hex,
            fingerprint_words,
            current_network_addable,
            current_network_reason,
        },
    )
    .await;
}

/// Short, user-facing note for the disabled "Add current network" control when
/// the current network cannot be fingerprinted as a trusted network — one concise
/// sentence per fail-closed [`network::NetworkFingerprintError`]
/// (contracts/settings-and-config.md). The settings webview falls back to a
/// generic note if this is ever absent, so the copy stays a best-effort hint.
fn lan_network_addable_reason(error: network::NetworkFingerprintError) -> String {
    match error {
        network::NetworkFingerprintError::NoDefaultRoute => {
            "This machine isn't connected to a local network, so it can't be trusted."
        }
        network::NetworkFingerprintError::ZeroGatewayMac => {
            "This network's router can't be identified yet — reconnect, then try again."
        }
        network::NetworkFingerprintError::VpnOnly => {
            "Only a VPN or tunnel is active, not a physical local network, so it can't be trusted."
        }
        network::NetworkFingerprintError::NoUsableSubnet => {
            "This network has no usable local subnet, so it can't be trusted."
        }
    }
    .to_owned()
}

/// The connection-level inputs a `Hello` carries into the feature-013 claim
/// path. Bundled into one value so the handler stays under Clippy's argument /
/// boolean-parameter thresholds and the claim intent travels as a unit.
struct HelloClaim<'a> {
    requested_window_id: Option<WindowId>,
    /// Spec 010 C7 clipboard-gating capability advertised by this client.
    clipboard_gating: bool,
    /// Feature 013: explicit takeover of a currently-connected window.
    takeover: bool,
    /// This connection's controller identity (local vs remote peer).
    controller: &'a ControllerIdentity,
}

async fn handle_client_hello(
    claim: HelloClaim<'_>,
    server: &IpcServerState,
    writer: &SharedWriter,
) -> WindowId {
    // Snapshot which windows have sessions, then resolve + register the
    // assignment atomically under a single `connected_clients` write lock
    // (see `resolve_and_register_claim`). The previous read-then-write split
    // was a TOCTOU race: a post-update reconnect burst could let two `Hello`s
    // for the same window both observe it free and both register.
    let all_windows = {
        let wm = server.workspace_manager.read().await;
        wm.window_ids_with_sessions()
    };

    let outcome = resolve_and_register_claim(
        WindowOwnership {
            connected: &server.connected_clients,
            controllers: &server.window_controllers,
            gating: &server.window_clipboard_gating,
        },
        &claim,
        &all_windows,
        writer,
    )
    .await;

    match outcome {
        ClaimOutcome::Owned { window_id, other_windows, displaced } => {
            // Feature 013: a takeover swapped out a live controller — notify it so
            // it freezes its last frame and offers reclaim (FR-007). This send is
            // the only takeover side effect left outside the claim lock: the
            // capability bit and controller identity were already re-bound to this
            // claimant atomically under the lock (see `resolve_and_register_claim`),
            // so no stale clipboard-gating or policy state can survive the swap
            // (FR-014), and the transition itself is traced there. The
            // clipboard-bridge routing then follows automatically — the new
            // controller's AttachSessions re-points each session's client writer
            // (T016), and the displaced client's later disconnect can no longer
            // detach them (see `detach_sessions`' ptr-eq guard).
            if let Some(displaced_writer) = displaced {
                send_message(&displaced_writer, &claim.controller.window_taken_over()).await;
            }

            if !other_windows.is_empty() {
                info!(%window_id, other_count = other_windows.len(), "Welcome includes other_windows — client will spawn additional processes");
            }
            let welcome =
                ServerMessage::Welcome { window_id, other_windows, clipboard_gating: true };
            send_message(writer, &welcome).await;

            info!(%window_id, client_clipboard_gating = claim.clipboard_gating, "client identified via Hello");
            window_id
        }
        ClaimOutcome::LostControl { window_id, controller: current } => {
            // Feature 013 auto-reconnect lost-control path (contracts/remote-
            // protocol.md): a remote client re-claimed (takeover = false) a
            // window another controller now holds. Complete the Welcome for the
            // requested id, then immediately displace it — no sessions attach and
            // the current controller keeps the window (never a silent seizure,
            // never a silent different-window). The client renders the standard
            // lost-control state and offers explicit reclaim (FR-011). The
            // connection is intentionally NOT registered, so its later teardown
            // leaves the current controller's writer + state untouched.
            let welcome = ServerMessage::Welcome {
                window_id,
                other_windows: Vec::new(),
                clipboard_gating: true,
            };
            send_message(writer, &welcome).await;
            send_message(writer, &current.window_taken_over()).await;
            info!(%window_id, "remote reconnect landed on a controlled window; sent lost-control");
            window_id
        }
    }
}

async fn handle_legacy_client(
    msg: ClientMessage,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> WindowId {
    let window_id = WindowId::new();
    // A fresh `WindowId::new()` cannot collide, so a direct insert is safe
    // here. Any path whose window ID *can* collide must go through
    // `resolve_and_register_claim` so the check and the insert stay atomic.
    server.connected_clients.write().await.insert(window_id, Arc::clone(writer));
    // Feature 013: legacy clients are always local; keep `window_controllers`
    // in lock-step with `connected_clients` so the window list treats the
    // window as locally controlled (no remote banner).
    server.window_controllers.write().await.insert(window_id, ControllerIdentity::Local);
    info!(%window_id, "legacy client (no Hello), assigned window");

    let mut context =
        ClientDispatchContext { server, writer, attached_ids, window_id, is_remote: false };
    dispatch_message(msg, &mut context).await;
    window_id
}

/// Outcome of resolving + registering a `Hello` window claim (feature 013).
/// `Owned` covers a fresh window, an adopted restart window, a local
/// no-takeover assignment, and a takeover swap (the latter carrying the
/// `displaced` writer to notify). `LostControl` is the remote auto-reconnect
/// landing on a window another controller still holds.
enum ClaimOutcome {
    Owned {
        window_id: WindowId,
        other_windows: Vec<WindowId>,
        /// The previous owner's writer when a takeover swapped it out; `None`
        /// for a fresh / adopted / first claim.
        displaced: Option<SharedWriter>,
    },
    LostControl {
        window_id: WindowId,
        /// Identity of the controller that keeps the window, to name in the
        /// immediate lost-control `WindowTakenOver` (FR-011).
        controller: ControllerIdentity,
    },
}

/// Pure decision (no locks, no side effects) for how a claim against the current
/// `connected` map resolves. Split out from [`resolve_and_register_claim`] so the
/// takeover / lost-control branching can be reasoned about in isolation.
enum ClaimResolution {
    /// Register `assigned` normally (fresh, adopted restart window, or a LOCAL
    /// no-takeover claim of a connected window that yields a different/new
    /// window — today's behavior, byte-for-byte).
    Assign { assigned: WindowId, other_windows: Vec<WindowId> },
    /// Take over a currently-connected window: swap its writer, displacing the
    /// current owner.
    Takeover { window_id: WindowId, other_windows: Vec<WindowId> },
    /// Remote non-takeover claim of a still-connected window: lost-control.
    LostControl { window_id: WindowId },
}

/// How a claim should treat a target window that is already connected
/// (feature 013). Derived from `takeover` + the connection's local/remote
/// transport so [`resolve_window_claim`] takes one mode instead of two bools.
#[derive(Clone, Copy)]
enum ClaimMode {
    /// Explicit takeover (picker attach or banner reclaim) — swap the writer.
    Takeover,
    /// Remote auto-reconnect — lost-control rather than a silent seize.
    RemoteReconnect,
    /// Local plain claim — assign a different/new window (today's behavior).
    LocalPlain,
}

/// Decide how a `Hello` claim resolves against the current `connected` map.
///
/// [`ClaimMode::Takeover`] of a connected window swaps the writer;
/// [`ClaimMode::RemoteReconnect`] onto a connected window is lost-control
/// (auto-reconnect convergence, never a silent seizure). Every other case —
/// including [`ClaimMode::LocalPlain`] onto a connected window — falls through to
/// [`resolve_window_assignment`], preserving today's behavior exactly.
fn resolve_window_claim<V>(
    hello_window_id: Option<WindowId>,
    mode: ClaimMode,
    windows_with_sessions: &HashSet<WindowId>,
    connected: &HashMap<WindowId, V>,
) -> ClaimResolution {
    if let Some(window_id) = hello_window_id
        && connected.contains_key(&window_id)
    {
        match mode {
            ClaimMode::Takeover => {
                // Explicit takeover: swap the connected window's writer.
                // `other_windows` mirrors an adopt of this window — the other
                // still-unconnected session windows.
                let other_windows = windows_with_sessions
                    .iter()
                    .filter(|wid| **wid != window_id && !connected.contains_key(wid))
                    .copied()
                    .collect();
                return ClaimResolution::Takeover { window_id, other_windows };
            }
            ClaimMode::RemoteReconnect => return ClaimResolution::LostControl { window_id },
            // Local, no takeover: fall through to the unchanged assignment path,
            // which assigns a different/new window (never displaces).
            ClaimMode::LocalPlain => {}
        }
    }
    let (assigned, other_windows) =
        resolve_window_assignment(hello_window_id, windows_with_sessions, connected);
    ClaimResolution::Assign { assigned, other_windows }
}

/// Feature 013: the three per-window ownership maps that move together on every
/// control transition, bundled into one `Copy` borrow so the claim path stays
/// under Clippy's argument threshold (mirrors [`ConnState`]). Always write-locked
/// in field order — `connected` → `controllers` → `gating` — so the nested
/// acquisition can never deadlock against detach or the window list.
#[derive(Clone, Copy)]
struct WindowOwnership<'a> {
    connected: &'a ConnectedClients,
    controllers: &'a WindowControllers,
    gating: &'a WindowClipboardGating,
}

/// Atomically resolve a window claim and register the connecting client's writer,
/// controller identity, AND clipboard-gating capability bit under one lock hold.
///
/// Holding the write locks across the check and the insert makes the claim
/// indivisible: concurrent `Hello`s for the same window can never both register
/// (the pre-013 TOCTOU race that a post-update reconnect burst triggers), and a
/// near-simultaneous takeover burst resolves deterministically to exactly one
/// controller. All three per-window maps move together so nothing can drift from
/// the writer slot: `window_controllers` carries the controller identity and
/// `window_clipboard_gating` the spec-010 capability bit (feature-013 T027). The
/// capability re-bind was previously done by the caller *after* the lock dropped,
/// which let a losing takeover overwrite the winner's bit — no stale clipboard
/// gating may survive a swap (FR-014), so it now happens here under the lock.
/// Lock order is `connected_clients` → `window_controllers` →
/// `window_clipboard_gating`, never the reverse (detach and the window list only
/// ever take these in that order or standalone, so no cycle exists).
///
/// - `Assign`: insert the resolved window + controller + capability bit (today's
///   behavior, now atomic with the capability bit).
/// - `Takeover`: swap the writer for the connected window, re-bind the controller
///   identity and capability bit to the new owner, and return the displaced
///   writer so the caller can send it `WindowTakenOver`.
/// - `LostControl`: leave the current owner untouched (this connection is NOT
///   registered, and the current controller's capability bit is preserved) and
///   report the current controller's identity to name.
///
/// Every claim that establishes or transfers ownership is traced after the lock
/// drops via [`log_control_transition`]; a `LostControl` landing changes no
/// ownership and is logged by the caller as the reconnect outcome instead.
///
/// `other_windows` is filtered against the same write-locked map so it never
/// lists a concurrently-registered window; `all_windows` is a brief snapshot
/// (see `resolve_window_assignment`) that can transiently under-fan-out but
/// never produce a duplicate window.
async fn resolve_and_register_claim(
    ownership: WindowOwnership<'_>,
    claim: &HelloClaim<'_>,
    all_windows: &HashSet<WindowId>,
    writer: &SharedWriter,
) -> ClaimOutcome {
    let mode = if claim.takeover {
        ClaimMode::Takeover
    } else if matches!(claim.controller, ControllerIdentity::Remote { .. }) {
        ClaimMode::RemoteReconnect
    } else {
        ClaimMode::LocalPlain
    };
    // Resolve, register the writer/controller, and re-bind the capability bit as
    // one indivisible transition, capturing the displaced controller (if any) for
    // the post-lock trace. The trace itself is emitted after the guards drop so a
    // slow blocking log writer never stalls a concurrent claim/detach/list.
    let (outcome, displaced_controller) = {
        let mut connected = ownership.connected.write().await;
        let mut controllers = ownership.controllers.write().await;
        let mut gating = ownership.gating.write().await;
        match resolve_window_claim(claim.requested_window_id, mode, all_windows, &connected) {
            ClaimResolution::Assign { assigned, other_windows } => {
                connected.insert(assigned, Arc::clone(writer));
                controllers.insert(assigned, claim.controller.clone());
                gating.insert(assigned, claim.clipboard_gating);
                (ClaimOutcome::Owned { window_id: assigned, other_windows, displaced: None }, None)
            }
            ClaimResolution::Takeover { window_id, other_windows } => {
                let displaced = connected.insert(window_id, Arc::clone(writer));
                let previous = controllers.insert(window_id, claim.controller.clone());
                gating.insert(window_id, claim.clipboard_gating);
                (ClaimOutcome::Owned { window_id, other_windows, displaced }, previous)
            }
            ClaimResolution::LostControl { window_id } => {
                let current =
                    controllers.get(&window_id).cloned().unwrap_or(ControllerIdentity::Local);
                (ClaimOutcome::LostControl { window_id, controller: current }, None)
            }
        }
    };

    log_control_transition(&outcome, claim.controller, displaced_controller.as_ref());
    outcome
}

/// Trace a resolved control transition (feature-013 T027). Fires for every claim
/// that establishes or transfers window ownership — a plain claim at `debug` and
/// a takeover at `info`, both naming the window and the controller identities so
/// a near-simultaneous claim burst leaves a legible ownership trail. A
/// `LostControl` landing transfers nothing and is intentionally silent here.
/// Kept off `REMOTE_AUDIT_TARGET`, whose taxonomy is the four canonical
/// accepted/refused/disconnect/severed lifecycle lines.
fn log_control_transition(
    outcome: &ClaimOutcome,
    new_controller: &ControllerIdentity,
    displaced: Option<&ControllerIdentity>,
) {
    match outcome {
        // A takeover is the only `Owned` outcome that displaces a live writer.
        ClaimOutcome::Owned { window_id, displaced: Some(_), .. } => {
            info!(
                %window_id,
                new_controller = %new_controller.transition_label(),
                displaced_controller =
                    %displaced.map_or(Cow::Borrowed("unknown"), ControllerIdentity::transition_label),
                "control transition: window taken over"
            );
        }
        ClaimOutcome::Owned { window_id, displaced: None, .. } => {
            debug!(
                %window_id,
                controller = %new_controller.transition_label(),
                "control transition: window claimed"
            );
        }
        ClaimOutcome::LostControl { .. } => {}
    }
}

/// Test-only shim preserving the pre-takeover `claim_window` shape so the
/// existing claim/registration invariant tests need no takeover/controller
/// plumbing. Production goes through [`resolve_and_register_claim`].
#[cfg(test)]
async fn claim_window(
    connected_clients: &ConnectedClients,
    requested_window_id: Option<WindowId>,
    all_windows: &HashSet<WindowId>,
    writer: &SharedWriter,
) -> (WindowId, Vec<WindowId>) {
    let controllers = new_window_controllers();
    let gating = new_window_clipboard_gating();
    let local = ControllerIdentity::Local;
    let claim = HelloClaim {
        requested_window_id,
        clipboard_gating: false,
        takeover: false,
        controller: &local,
    };
    match resolve_and_register_claim(
        WindowOwnership {
            connected: connected_clients,
            controllers: &controllers,
            gating: &gating,
        },
        &claim,
        all_windows,
        writer,
    )
    .await
    {
        ClaimOutcome::Owned { window_id, other_windows, .. } => (window_id, other_windows),
        ClaimOutcome::LostControl { window_id, .. } => (window_id, Vec::new()),
    }
}

/// Release a window's connected-client entry only if it still belongs to
/// `writer`. The `SharedWriter` Arc is the connection's identity token: a
/// stale disconnect from a client already superseded by a newer client for
/// the same window must not evict the new owner — doing so makes the window
/// look unconnected and triggers a duplicate respawn. Returns whether the
/// registry is now empty, for settings-shutdown scheduling.
fn release_window_if_owned(
    connected: &mut HashMap<WindowId, SharedWriter>,
    window_id: WindowId,
    writer: &SharedWriter,
) -> bool {
    if connected.get(&window_id).is_some_and(|current| Arc::ptr_eq(current, writer)) {
        connected.remove(&window_id);
    }
    connected.is_empty()
}

/// Feature 013 takeover-authorization guard: whether `writer` is STILL the
/// registered controller of `window_id`. The connection's `SharedWriter` is its
/// identity token (same `Arc::ptr_eq` test as [`release_window_if_owned`] and
/// [`detach_sessions`]), so a connection that a takeover displaced — its window
/// re-bound to another controller under the claim lock — fails this and is barred
/// from mutating the window or re-attaching its sessions, even though its own
/// never-revoked `attached_ids` still names those sessions. A local Unix-socket
/// client is always its own window's registered controller (a local no-takeover
/// claim assigns a *different* window rather than displacing one), so this is a
/// no-op for the local path; a `LostControl` reconnect (never registered) also
/// fails it, as intended.
async fn connection_controls_window(
    connected: &ConnectedClients,
    window_id: WindowId,
    writer: &SharedWriter,
) -> bool {
    connected.read().await.get(&window_id).is_some_and(|current| Arc::ptr_eq(current, writer))
}

/// Whether a client message mutates a window's live session state (or reads its
/// scrollback) and therefore requires the sender to still be that window's
/// registered controller (feature 013 takeover authorization). `AttachSessions`
/// is intentionally excluded — its stricter per-session ownership PLUS controller
/// check lives in [`filter_attachable_sessions`] so it can deny per session and
/// reply with an error.
fn requires_window_control(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::KeyInput { .. }
            | ClientMessage::Resize { .. }
            | ClientMessage::CloseSession { .. }
            | ClientMessage::CloseWindow { .. }
            | ClientMessage::FocusChanged { .. }
            | ClientMessage::SearchRequest { .. }
    )
}

/// Await a connection's sever signal, or never resolve when there is none (local
/// connections). Lets the shared read paths `select!` on severing uniformly.
async fn await_sever(sever_rx: Option<&mut tokio::sync::oneshot::Receiver<()>>) {
    match sever_rx {
        Some(rx) => drop(rx.await),
        None => std::future::pending::<()>().await,
    }
}

/// Why [`run_client_message_loop`] returned: a normal peer disconnect / read
/// error, or a feature-013 sever because remote access was disabled (T023). The
/// caller runs the same detach cleanup for both; only the audit line differs.
#[derive(Clone, Copy)]
enum LoopExit {
    /// The peer closed the connection or the read errored.
    Disconnected,
    /// Remote access was disabled; the connection is being severed (FR-016).
    Severed,
}

async fn run_client_message_loop<R>(
    reader: &mut R,
    window_id: WindowId,
    conn: ConnState<'_>,
    mut sever_rx: Option<tokio::sync::oneshot::Receiver<()>>,
) -> LoopExit
where
    R: tokio::io::AsyncRead + Unpin,
{
    let is_remote = sever_rx.is_some();
    loop {
        // Race each client-message read against the sever signal so a disable
        // (T023) drops a remote connection out of its loop; on sever, fall through
        // to the caller's normal detach cleanup. Local connections pass no sever
        // channel, so the sever arm never fires. The read also carries the remote
        // idle-read timeout so a vanished peer's slot is reclaimed (local reads
        // stay untimed).
        let read = tokio::select! {
            biased;
            () = await_sever(sever_rx.as_mut()) => return LoopExit::Severed,
            read = read_client_frame(reader, is_remote) => read,
        };
        let msg: ClientMessage = match read {
            Some(Ok(msg)) => msg,
            None => {
                debug!(%window_id, "remote connection idle past timeout; disconnecting");
                return LoopExit::Disconnected;
            }
            Some(Err(ScribeError::Io { .. })) => {
                debug!(%window_id, "client disconnected");
                return LoopExit::Disconnected;
            }
            Some(Err(e)) => {
                warn!(%window_id, "failed to read client message: {e}");
                return LoopExit::Disconnected;
            }
        };

        let mut context = ClientDispatchContext {
            server: conn.server,
            writer: conn.writer,
            attached_ids: conn.attached_ids,
            window_id,
            is_remote,
        };
        dispatch_message(msg, &mut context).await;
    }
}

async fn detach_client_window(
    window_id: WindowId,
    server: &IpcServerState,
    attached_ids: &AttachedSessionIds,
    writer: &SharedWriter,
) {
    let attached_ids = attached_snapshot(attached_ids).await;
    // Detach only the sessions still routed to THIS connection. A feature-013
    // takeover may already have re-pointed them at the new controller, whose
    // output + clipboard-bridge routing (T016) must survive this old client's
    // disconnect; the ptr-eq guard inside `detach_sessions` enforces that.
    detach_sessions(&server.live_sessions, &attached_ids, writer).await;

    // Determine ownership and release under one lock. A stale disconnect from a
    // client already superseded by a takeover (or a reconnect race) must not
    // evict the new owner — `release_window_if_owned` re-checks the same
    // `Arc::ptr_eq` guard.
    let (still_owned, last_client_disconnected) = {
        let mut connected = server.connected_clients.write().await;
        let still_owned =
            connected.get(&window_id).is_some_and(|current| Arc::ptr_eq(current, writer));
        let empty = release_window_if_owned(&mut connected, window_id, writer);
        (still_owned, empty)
    };

    // Only tear down this window's per-connection state when THIS client still
    // owned it. After a takeover the window belongs to the new controller, whose
    // clipboard-gating bit and controller identity must NOT be dropped (FR-014).
    if still_owned {
        // Spec 010 C7: drop the cached clipboard-gating bit so a reconnecting
        // client is re-evaluated at its next Hello.
        server.window_clipboard_gating.write().await.remove(&window_id);
        // Feature 013 (T027): trace the Controlled → Unconnected transition,
        // naming the controller whose window just became reattachable.
        if let Some(released) = server.window_controllers.write().await.remove(&window_id) {
            debug!(
                %window_id,
                controller = %released.transition_label(),
                "control transition: window released"
            );
        }
    }
    info!(%window_id, still_owned, "client connection closed; window released if still owned");
    if last_client_disconnected {
        schedule_settings_shutdown_if_no_clients(Arc::clone(&server.connected_clients));
    }
}

/// Clear the client writer for each session so output stops being forwarded —
/// but ONLY for sessions still routed to `writer`. Sessions remain alive in the
/// registry for future client attachment. The `Arc::ptr_eq` guard is the
/// session-level analog of `release_window_if_owned`: after a feature-013
/// takeover a session already points at the new controller, and this old
/// client's disconnect must not detach it (which would break the new
/// controller's output + clipboard-bridge routing, FR-014).
async fn detach_sessions(
    live_sessions: &LiveSessionRegistry,
    ids: &HashSet<SessionId>,
    writer: &SharedWriter,
) {
    let sessions = live_sessions.read().await;
    for id in ids {
        if let Some(session) = sessions.get(id) {
            let mut client_writer = session.client_writer.lock().await;
            if client_writer.as_ref().is_some_and(|current| Arc::ptr_eq(current, writer)) {
                *client_writer = None;
                drop(client_writer);
                clear_session_attachment(&session.attachment).await;
                info!(%id, "session detached (client disconnected)");
            }
        }
    }
}

/// Close the singleton settings window once the client registry stays empty
/// long enough to rule out a hot-reload or reconnect race.
fn schedule_settings_shutdown_if_no_clients(connected_clients: ConnectedClients) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if connected_clients.read().await.is_empty() {
            quit_settings_process();
        } else {
            debug!("settings shutdown skipped because a client reconnected");
        }
    });
}

/// Ask the standalone settings process to quit, if it is running.
fn quit_settings_process() {
    let socket_path = scribe_common::socket::settings_socket_path();
    match std::os::unix::net::UnixStream::connect(&socket_path) {
        Ok(mut stream) => {
            use std::io::Write as _;

            if let Err(e) = stream.write_all(b"{\"cmd\":\"quit\"}\n") {
                warn!("failed to send quit command to settings: {e}");
            } else {
                debug!("sent quit to settings process after last client disconnect");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            debug!("settings quit skipped because socket connect failed: {e}");
        }
    }
}

/// Extract per-workspace session order from a reported tree and apply it.
fn apply_tab_order_from_tree(wm: &mut WorkspaceManager, tree: &WorkspaceTreeNode) {
    match tree {
        WorkspaceTreeNode::Leaf { workspace_id, session_ids, .. } => {
            if !session_ids.is_empty() {
                wm.reorder_sessions(*workspace_id, session_ids);
            }
        }
        WorkspaceTreeNode::Split { first, second, .. } => {
            apply_tab_order_from_tree(wm, first);
            apply_tab_order_from_tree(wm, second);
        }
    }
}

/// Dispatch a single `ClientMessage` to the appropriate handler.
async fn dispatch_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    // Feature 013 takeover authorization: a connection displaced by a takeover
    // keeps its stale, never-revoked `attached_ids`, so the per-session
    // `attached_contains` gate alone would still let it mutate the window it lost
    // (KeyInput/Resize/CloseSession/CloseWindow/FocusChanged) or read its
    // scrollback (SearchRequest). Bar those unless this connection is STILL the
    // window's registered controller. Local Unix-socket clients always are, so
    // this never affects the local path; `AttachSessions` is guarded separately in
    // `filter_attachable_sessions`.
    if requires_window_control(&msg)
        && !connection_controls_window(
            &context.server.connected_clients,
            context.window_id,
            context.writer,
        )
        .await
    {
        debug!(
            window_id = %context.window_id,
            "ignoring window-control message from a displaced/non-controller connection"
        );
        return;
    }
    match msg {
        msg @ (ClientMessage::CreateSession { .. }
        | ClientMessage::KeyInput { .. }
        | ClientMessage::CloseSession { .. }
        | ClientMessage::Resize { .. }
        | ClientMessage::AttachSessions { .. }
        | ClientMessage::ConfigReloaded
        | ClientMessage::FocusChanged { .. }
        | ClientMessage::SearchRequest { .. }) => {
            dispatch_session_message(msg, context).await;
        }
        msg @ (ClientMessage::Subscribe { .. }
        | ClientMessage::RequestSnapshot { .. }
        | ClientMessage::CreateWorkspace
        | ClientMessage::ListSessions
        | ClientMessage::WorkspaceNotesGet { .. }
        | ClientMessage::WorkspaceNotesMutate { .. }
        | ClientMessage::ReportWorkspaceTree { .. }) => {
            dispatch_workspace_message(msg, context).await;
        }
        msg @ (ClientMessage::CloseWindow { .. }
        | ClientMessage::QuitAll
        | ClientMessage::TriggerUpdate
        | ClientMessage::DismissUpdate
        | ClientMessage::CheckForUpdates
        | ClientMessage::ListReleases
        | ClientMessage::ListWindows
        | ClientMessage::DispatchAction { .. }
        | ClientMessage::EnvPreflight) => {
            dispatch_window_message(msg, context).await;
        }
        ClientMessage::Hello { .. } => debug!("unexpected Hello after handshake, ignoring"),
        ClientMessage::ClipboardPromptResponse { request_id, decision } => {
            forward_clipboard_command_to_window(
                context,
                ClipboardCommand::PromptResponse { request_id, decision },
            )
            .await;
        }
        ClientMessage::ClipboardBridgeReadReply { request_id, payload } => {
            forward_clipboard_command_to_window(
                context,
                ClipboardCommand::BridgeReadReply { request_id, payload },
            )
            .await;
        }
        ClientMessage::LanApprovalDecision { request_id, approve } => {
            handle_lan_approval_decision(context, request_id, approve);
        }
        other => debug!(?other, "unhandled client message"),
    }
}

/// Deliver the owning user's decision on a pending LAN device approval
/// (feature 014, contracts/lan-protocol.md). Local-socket only: the GUI answers
/// the prompt, so a decision arriving over any remote transport (a peer past
/// `Hello`) is IGNORED — a remote device must never approve another device's
/// pending connection. Correlates by `request_id` via
/// [`PendingApprovals::resolve`]; a stale/duplicate id is a harmless no-op.
fn handle_lan_approval_decision(
    context: &ClientDispatchContext<'_>,
    request_id: u64,
    approve: bool,
) {
    if context.is_remote {
        debug!(request_id, "ignoring LanApprovalDecision from a remote connection");
        return;
    }
    context.server.remote_control.pending_approvals.resolve(request_id, approve);
}

/// Route a `ClipboardCommand` to the originating session's PTY reader task.
/// Spec 010 contract C4: the dispatcher uses the `attached_ids` snapshot to
/// find the live session(s) belonging to this window — the response is
/// matched by `request_id` inside the reader task so a stale or rogue reply
/// is harmless.
async fn forward_clipboard_command_to_window(
    context: &mut ClientDispatchContext<'_>,
    cmd: ClipboardCommand,
) {
    let attached = attached_snapshot(context.attached_ids).await;
    if attached.is_empty() {
        debug!(window_id = %context.window_id, "clipboard reply dropped: no attached sessions");
        return;
    }
    let sessions = context.server.live_sessions.read().await;
    // Fan out the command to every session the window is attached to.
    // Each reader task ignores commands whose `request_id` it never issued
    // so this fan-out is safe even when the window owns multiple panes.
    for session_id in &attached {
        if let Some(session) = sessions.get(session_id)
            && session.clipboard_command_tx.send(reclone_clipboard_command(&cmd)).is_err()
        {
            debug!(%session_id, "clipboard command channel closed");
        }
    }
}

fn reclone_clipboard_command(cmd: &ClipboardCommand) -> ClipboardCommand {
    match cmd {
        ClipboardCommand::PromptResponse { request_id, decision } => {
            ClipboardCommand::PromptResponse { request_id: *request_id, decision: *decision }
        }
        ClipboardCommand::BridgeReadReply { request_id, payload } => {
            ClipboardCommand::BridgeReadReply { request_id: *request_id, payload: payload.clone() }
        }
        ClipboardCommand::RefreshPolicy { policy } => {
            ClipboardCommand::RefreshPolicy { policy: policy.clone() }
        }
    }
}

async fn dispatch_session_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::CreateSession {
            workspace_id,
            split_direction,
            cwd,
            size,
            command,
            env_envelope_id,
        } => {
            handle_create_session(
                CreateSessionRequest {
                    workspace_id,
                    split_direction,
                    cwd,
                    size,
                    command,
                    env_envelope_id,
                },
                context,
            )
            .await;
        }
        ClientMessage::KeyInput { session_id, data, dismisses_attention } => {
            handle_key_input(
                session_id,
                &data,
                dismisses_attention,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
        }
        ClientMessage::CloseSession { session_id } => {
            handle_close_session(
                session_id,
                &context.server.workspace_manager,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
            context.server.workspace_manager.write().await.remove_session_from_window(session_id);
        }
        ClientMessage::Resize { session_id, size } => {
            handle_resize(session_id, size, &context.server.live_sessions, context.attached_ids)
                .await;
        }
        ClientMessage::AttachSessions { session_ids, dimensions } => {
            handle_attach_sessions(&session_ids, &dimensions, context).await;
        }
        ClientMessage::ConfigReloaded => {
            handle_config_reloaded(context.server).await;
        }
        ClientMessage::FocusChanged { gained, lost } => {
            handle_focus_changed(gained, lost, &context.server.live_sessions, context.attached_ids)
                .await;
        }
        ClientMessage::SearchRequest { session_id, query, limit } => {
            handle_search_request(session_id, query, limit, context).await;
        }
        other => debug!(?other, "ignored non-session client message in session dispatcher"),
    }
}

async fn dispatch_workspace_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::Subscribe { session_ids } => {
            let cap = session_ids.len().min(MAX_SUBSCRIBE_IDS);
            let ids = session_ids.get(..cap).unwrap_or(&session_ids);
            handle_subscribe(
                ids,
                &context.server.workspace_manager,
                context.writer,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
        }
        ClientMessage::RequestSnapshot { session_id } => {
            handle_request_snapshot(
                session_id,
                context.writer,
                &context.server.live_sessions,
                context.attached_ids,
            )
            .await;
        }
        ClientMessage::CreateWorkspace => {
            handle_create_workspace(&context.server.workspace_manager, context.writer).await;
        }
        ClientMessage::ListSessions => {
            handle_list_sessions(
                &context.server.live_sessions,
                &context.server.workspace_manager,
                context.writer,
                context.window_id,
            )
            .await;
        }
        ClientMessage::WorkspaceNotesGet { workspace_ids } => {
            handle_workspace_notes_get(
                &context.server.workspace_notes,
                context.writer,
                &workspace_ids,
            )
            .await;
        }
        ClientMessage::WorkspaceNotesMutate { mutation } => {
            handle_workspace_notes_mutate(
                &context.server.workspace_notes,
                &context.server.connected_clients,
                context.writer,
                mutation,
            )
            .await;
        }
        ClientMessage::ReportWorkspaceTree { tree } => {
            debug!(window_id = %context.window_id, "received workspace tree from client");
            let mut wm = context.server.workspace_manager.write().await;
            apply_tab_order_from_tree(&mut wm, &tree);
            wm.set_workspace_tree(tree.clone());
            wm.set_window_tree(context.window_id, tree);
        }
        other => debug!(?other, "ignored non-workspace client message in workspace dispatcher"),
    }
}

async fn dispatch_window_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::CloseWindow { window_id: target_window } => {
            handle_close_window(target_window, context).await;
        }
        ClientMessage::QuitAll => {
            handle_quit_all(
                context.window_id,
                &context.server.connected_clients,
                &context.server.workspace_manager,
            )
            .await;
        }
        ClientMessage::TriggerUpdate => {
            info!(window_id = %context.window_id, "client triggered update");
            context.server.updater_handle.trigger();
        }
        ClientMessage::DismissUpdate => {
            info!(window_id = %context.window_id, "client dismissed update notification");
            context.server.updater_handle.dismiss();
        }
        ClientMessage::CheckForUpdates => {
            handle_check_for_updates(context).await;
        }
        ClientMessage::ListReleases => {
            handle_list_releases_msg(context).await;
        }
        ClientMessage::ListWindows => {
            handle_list_windows(
                &context.server.connected_clients,
                &context.server.window_controllers,
                &context.server.workspace_manager,
                context.writer,
            )
            .await;
        }
        ClientMessage::DispatchAction { window_id: target_window_id, action } => {
            handle_dispatch_action(
                target_window_id,
                action,
                &context.server.connected_clients,
                context.window_id,
                context.writer,
            )
            .await;
        }
        ClientMessage::EnvPreflight => {
            handle_env_preflight(context.writer).await;
        }
        other => debug!(?other, "ignored non-window client message in window dispatcher"),
    }
}

/// Helper extracted from [`dispatch_window_message`] to keep the dispatcher's
/// cognitive complexity under Clippy's threshold.
async fn handle_check_for_updates(context: &mut ClientDispatchContext<'_>) {
    info!(window_id = %context.window_id, "client requested manual update check");
    let state = context.server.updater_handle.request_check().await;
    send_message(context.writer, &ServerMessage::UpdateCheckResult { state }).await;
}

/// Helper extracted from [`dispatch_window_message`] to keep the dispatcher's
/// cognitive complexity under Clippy's threshold.
async fn handle_list_releases_msg(context: &mut ClientDispatchContext<'_>) {
    info!(window_id = %context.window_id, "client requested release list");
    let state = crate::releases::handle_list_releases(
        &context.server.release_catalog,
        &context.server.release_fetcher,
    )
    .await;
    send_message(context.writer, &ServerMessage::ReleaseList { state }).await;
}

/// Create a new PTY session, register it, start the reader task.
async fn handle_create_session(
    request: CreateSessionRequest,
    context: &mut ClientDispatchContext<'_>,
) {
    let session_id = match context
        .server
        .session_manager
        .create_session(SessionLaunchRequest {
            workspace_id: request.workspace_id,
            window_id: context.window_id,
            cwd: request.cwd,
            size: request.size,
            command: request.command,
            env_envelope_id: request.env_envelope_id,
        })
        .await
    {
        Ok(id) => id,
        Err(e) => {
            send_error(context.writer, &format!("failed to create session: {e}")).await;
            return;
        }
    };

    // Register session with workspace manager.  When `split_direction` is
    // `Some` the workspace is auto-created (client just split the window).
    {
        let mut wm = context.server.workspace_manager.write().await;
        wm.add_session(request.workspace_id, session_id, request.split_direction);
        wm.assign_session_to_window(context.window_id, session_id);
    }

    let Some(session) = context.server.session_manager.take_session(session_id).await else {
        send_error(context.writer, "session vanished after creation").await;
        return;
    };

    // Notify client of session creation.
    let creation_msg = ServerMessage::SessionCreated {
        session_id,
        workspace_id: request.workspace_id,
        shell_name: session.shell_name.clone(),
    };
    send_message(context.writer, &creation_msg).await;

    // Send workspace info so the client knows the accent color and name.
    {
        let wm = context.server.workspace_manager.read().await;
        if let Some((name, accent_color, ws_split_dir, project_root)) =
            wm.workspace_info(request.workspace_id)
        {
            let info_msg = ServerMessage::WorkspaceInfo {
                workspace_id: request.workspace_id,
                name,
                accent_color,
                split_direction: ws_split_dir,
                project_root,
            };
            send_message(context.writer, &info_msg).await;
        }
    }

    start_session(
        StartSessionIds {
            session: session_id,
            workspace: request.workspace_id,
            window: context.window_id,
        },
        session,
        InitialAttachment {
            writer: Some(context.writer),
            attached_ids: Some(context.attached_ids),
        },
        SessionRuntimeContext {
            workspace_manager: &context.server.workspace_manager,
            live_sessions: &context.server.live_sessions,
            window_clipboard_gating: &context.server.window_clipboard_gating,
        },
    )
    .await;
    attached_insert(context.attached_ids, session_id).await;
}

/// Split a `ManagedSession`, register in the live registry, and start
/// the PTY reader task. When `writer` is `None` the session starts in
/// detached mode (PTY reader runs but output is silently discarded until
/// a client attaches).
///
/// The registry insert is performed synchronously (before the PTY reader
/// task is spawned) to eliminate the race where `CloseSession` could arrive
/// before the session is visible in the registry.
async fn start_session(
    ids: StartSessionIds,
    session: ManagedSession,
    initial_attachment: InitialAttachment<'_>,
    runtime: SessionRuntimeContext<'_>,
) {
    let StartSessionIds { session: session_id, workspace: workspace_id, window: window_id } = ids;
    #[rustfmt::skip]
    let ManagedSession {
        pty_fd, resize_fd, child_pid, term, ansi_processor, osc_parser,
        event_rx, shell_name, pty, handoff_snapshot, task_label, title, cwd,
        context, ai_state, ai_provider_hint, cell_width, cell_height,
        env_envelope_id, ..
    } = session;
    let resize_fd = Arc::new(resize_fd);
    let (pty_read, pty_write) = tokio::io::split(pty_fd);
    let pty_write = Arc::new(Mutex::new(pty_write));

    // Wrap the client writer in an optional so the reader task can
    // continue running when the client disconnects.
    let client_writer = Arc::new(Mutex::new(initial_attachment.writer.map(Arc::clone)));
    let attachment = Arc::new(Mutex::new(initial_attachment.attached_ids.map(Arc::clone)));

    let (clipboard_command_tx, clipboard_command_rx) = new_clipboard_command_channel();

    let preserve_ai_scrollback = Arc::new(AtomicBool::new(load_preserve_ai_scrollback_setting()));
    let scrollback_lines = Arc::new(AtomicUsize::new(load_scrollback_lines_setting()));
    let ai_provider = ai_state.as_ref().map(|state| state.provider).or(ai_provider_hint);

    let live = LiveSession {
        pty_write: Arc::clone(&pty_write),
        resize_fd,
        term: Arc::clone(&term),
        child_pid,
        client_writer: Arc::clone(&client_writer),
        attachment: Arc::clone(&attachment),
        workspace_id,
        shell_name,
        title: title.unwrap_or_else(|| String::from("shell")),
        task_label,
        cwd,
        context,
        ai_state,
        ai_provider_hint,
        cell_width,
        cell_height,
        pty,
        handoff_snapshot,
        preserve_ai_scrollback: Arc::clone(&preserve_ai_scrollback),
        scrollback_lines: Arc::clone(&scrollback_lines),
        env_window_id: window_id,
        env_envelope_id,
        clipboard_command_tx,
    };
    runtime.live_sessions.write().await.insert(session_id, live);
    let clipboard_policy = load_clipboard_policy_snapshot();

    let state = PtyReaderState {
        session_id,
        window_id,
        child_pid,
        pty_read,
        pty_write,
        term,
        ansi_processor,
        osc_parser,
        event_rx,
        client_writer,
        attachment,
        workspace_manager: Arc::clone(runtime.workspace_manager),
        live_sessions: Arc::clone(runtime.live_sessions),
        window_clipboard_gating: Arc::clone(runtime.window_clipboard_gating),
        clipboard_burst: crate::clipboard_state::ClipboardBurstState::new(clipboard_policy),
        pending_clipboard_reads: HashMap::new(),
        pending_clipboard_prompt: None,
        clipboard_command_rx,
        osc_events: Vec::new(),
        last_proc_cwd: None,
        ed3_filter: Ed3Filter::new(),
        claude_picker_filter: ClaudePickerTruncationFilter::new(),
        lf_crlf_filter: LfCrlfFilter::new(),
        ai_provider,
        cell_width,
        cell_height,
        preserve_ai_scrollback,
        scrollback_lines,
        preserved_ai_scrollback: PreservedAiScrollback::default(),
        pending_ai_scrollback_baseline: false,
    };

    tokio::spawn(pty_reader_task(state));
}

/// Spec 010 C4: build the OSC 52 client→PTY-reader control channel.
/// Unbounded because each session emits at most one prompt-in-flight
/// at a time and the queue depth is naturally bounded by the burst
/// guard.
fn new_clipboard_command_channel() -> (
    tokio::sync::mpsc::UnboundedSender<ClipboardCommand>,
    tokio::sync::mpsc::UnboundedReceiver<ClipboardCommand>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Spec 010: snapshot the OSC 52 policy at session-creation time. The
/// resulting `ClipboardBurstState` is dropped together with the reader
/// task on session exit; later live reloads land via
/// `ConfigReloaded` → `ClipboardCommand::RefreshPolicy`.
fn load_clipboard_policy_snapshot() -> scribe_common::config::ClipboardPolicyConfig {
    scribe_config::load_config().map(|cfg| cfg.terminal.clipboard_policy).unwrap_or_default()
}

#[cfg(test)]
fn ai_state_uses_ed3_filter(ai_state: Option<&AiProcessState>) -> bool {
    ai_state.is_some_and(|state| ai_provider_uses_ed3_filter(Some(state.provider)))
}

fn ai_provider_uses_ed3_filter(ai_provider: Option<AiProvider>) -> bool {
    ai_provider.is_some_and(|provider| AiProvider::all().contains(&provider))
}

/// True when the Claude picker truncation workaround should run for this
/// session. Only Claude Code's Ink-based `AskUserQuestion` picker emits the
/// 18-byte truncation signature this filter targets.
fn ai_provider_uses_claude_picker_filter(ai_provider: Option<AiProvider>) -> bool {
    matches!(ai_provider, Some(AiProvider::ClaudeCode))
}

/// Write key input data to the PTY.
async fn handle_key_input(
    session_id: SessionId,
    data: &[u8],
    dismisses_attention: bool,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent KeyInput for unattached session");
        return;
    }

    if data.len() > MAX_KEY_INPUT_BYTES {
        warn!(
            %session_id,
            len = data.len(),
            max = MAX_KEY_INPUT_BYTES,
            "KeyInput payload too large, dropping"
        );
        return;
    }

    let pty_write = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            warn!(%session_id, "KeyInput for unknown session");
            return;
        };

        if dismisses_attention {
            dismiss_persisted_attention_state(session);
        }

        Arc::clone(&session.pty_write)
    };

    let mut pty_write = pty_write.lock().await;
    if let Err(e) = pty_write.write_all(data).await {
        warn!(%session_id, "failed to write to PTY: {e}");
    }
}

fn dismiss_persisted_attention_state(session: &mut LiveSession) {
    let Some(ai_state) = session.ai_state.as_ref() else { return };
    let provider = ai_state.provider;
    if matches!(
        ai_state.state,
        AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
    ) {
        session.ai_provider_hint = Some(provider);
        session.ai_state = None;
    }
}

/// Return `true` when the session has `TermMode::FOCUS_IN_OUT` active.
async fn session_has_focus_mode(session: &LiveSession) -> bool {
    let term = session.term.lock().await;
    term.mode().contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT)
}

/// Write a CSI focus byte sequence to a session's PTY if it has opted in.
async fn send_focus_event(session: &LiveSession, bytes: &[u8]) {
    if session_has_focus_mode(session).await {
        let mut pty_write = session.pty_write.lock().await;
        if let Err(e) = pty_write.write_all(bytes).await {
            debug!("focus event write failed: {e}");
        }
    }
}

/// Send CSI focus events to PTY sessions that have DECSET 1004 enabled.
///
/// When a session has `TermMode::FOCUS_IN_OUT` active, write `\x1b[I`
/// (focus gained) or `\x1b[O` (focus lost) to the PTY so the
/// application can respond (e.g. hide cursor, reduce animation).
async fn handle_focus_changed(
    gained: Option<SessionId>,
    lost: Option<SessionId>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    let sessions = live_sessions.read().await;
    if let Some(lost_id) = lost {
        if attached_contains(attached_ids, lost_id).await {
            if let Some(session) = sessions.get(&lost_id) {
                send_focus_event(session, b"\x1b[O").await;
            }
        }
    }
    if let Some(gained_id) = gained {
        if attached_contains(attached_ids, gained_id).await {
            if let Some(session) = sessions.get(&gained_id) {
                send_focus_event(session, b"\x1b[I").await;
            }
        }
    }
}

/// Send `SIGHUP` to the child process of a handoff-restored session.
///
/// After a hot-reload handoff the `pty` field is `None` because we only
/// received the master fd via `SCM_RIGHTS`, not the original `Pty` object.
/// Without the `Pty`, dropping the `LiveSession` does not send `SIGHUP`
/// to the child. This helper fills that gap so `CloseSession` and
/// `CloseWindow` can clean up handoff-restored sessions correctly.
fn signal_if_handoff_session(session_id: SessionId, session: &LiveSession) {
    if session.pty.is_some() {
        return; // `Pty::Drop` will send SIGHUP.
    }
    let pid = session.child_pid.cast_signed();
    info!(%session_id, pid, "sending SIGHUP to handoff-restored session");
    if let Err(err) = kill(Pid::from_raw(pid), Signal::SIGHUP) {
        warn!(%session_id, pid, %err, "failed to send SIGHUP to child");
    }
}

/// Close a session and clean up. For fresh sessions the `Pty::Drop` inside
/// `LiveSession` sends SIGHUP to the child process; for handoff-restored
/// sessions (`pty: None`) we send SIGHUP explicitly so the child is not
/// leaked. The PTY reader task exits naturally on EOF once the child dies.
///
/// Per T019, on this clean-close path we also delete the session's encrypted
/// env envelope (`<state_dir>/restore/env/<window_id>/<launch_id>.envz`) plus
/// its keystore DEK. The PTY-EOF / child-exit path in [`finalize_pty_reader`]
/// deliberately does NOT delete the envelope: a session that died because the
/// user typed `exit` (or because the shell crashed) is still eligible for
/// cold-restart restore, so the envelope must remain on disk until the user
/// explicitly issues a `CloseSession`.
async fn handle_close_session(
    session_id: SessionId,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent CloseSession for unattached session");
        return;
    }

    let removed = live_sessions.write().await.remove(&session_id);
    // Capture envelope coordinates before the session value is dropped so
    // we can fire the delete after SIGHUP cleanup. Cloned (rather than
    // moved) to keep the existing `drop(removed)` step that triggers
    // `Pty::Drop` for fresh sessions.
    let envelope_coords = removed
        .as_ref()
        .and_then(|s| s.env_envelope_id.as_ref().map(|id| (s.env_window_id, id.clone())));
    if let Some(session) = &removed {
        signal_if_handoff_session(session_id, session);
    }
    // `removed` is dropped here — if `pty` is `Some`, `Pty::Drop` sends SIGHUP.
    drop(removed);
    workspace_manager.write().await.remove_session(session_id);
    attached_remove(attached_ids, session_id).await;

    // Best-effort envelope + DEK delete. `delete_envelope` is idempotent
    // and swallows `NotFound`, so it's safe to call when the feature was
    // off at create time (no envelope ever existed) or when the persist
    // scheduler had not yet flushed a first write. Failures are logged
    // but do not block the close.
    if let Some((window_id, launch_id)) = envelope_coords {
        if let Err(err) = crate::env_store::store::delete_envelope(window_id, &launch_id).await {
            warn!(
                target: "scribe_server::ipc_server",
                %session_id,
                %window_id,
                %launch_id,
                error = ?err,
                "env-envelope delete failed during CloseSession"
            );
        }
    }

    info!(%session_id, "session closed by client");
}

/// Close a window: destroy every session it owns and remove the window from
/// the workspace manager so it won't be resurrected on the next client launch.
///
/// Per T019, also sweeps every env envelope under the closing window's
/// `restore/env/<window_id>/` directory (and the matching keystore DEKs).
/// This is the "clean close" path; the PTY-EOF / child-exit path in
/// [`finalize_pty_reader`] deliberately preserves envelopes so they remain
/// available for cold-restart restore.
async fn handle_close_window(window_id: WindowId, context: &ClientDispatchContext<'_>) {
    if window_id != context.window_id {
        send_error(context.writer, &format!("cannot close another window: {window_id}")).await;
        return;
    }

    let session_ids = context.server.workspace_manager.read().await.sessions_for_window(window_id);
    info!(%window_id, count = session_ids.len(), "closing window — destroying sessions");

    // Destroy each session. For fresh sessions `Pty::Drop` sends SIGHUP;
    // for handoff-restored sessions (`pty: None`) we signal explicitly.
    {
        let mut sessions = context.server.live_sessions.write().await;
        for &sid in &session_ids {
            if let Some(session) = sessions.remove(&sid) {
                signal_if_handoff_session(sid, &session);
                // `session` dropped here — `Pty::Drop` fires if `pty` is `Some`.
            }
            attached_remove(context.attached_ids, sid).await;
        }
    }

    // Remove window and all session→window mappings.
    let mut wm = context.server.workspace_manager.write().await;
    for &sid in &session_ids {
        wm.remove_session(sid);
    }
    wm.remove_window(window_id);
    drop(wm);

    // Best-effort envelope sweep for the whole window. Idempotent — a
    // missing per-window dir is treated as success. Failures are logged
    // but do not block the WindowClosed reply.
    if let Err(err) = crate::env_store::store::delete_window_envelopes(window_id).await {
        warn!(
            target: "scribe_server::ipc_server",
            %window_id,
            error = ?err,
            "env-envelope window sweep failed during CloseWindow"
        );
    }

    send_message(context.writer, &ServerMessage::WindowClosed { window_id }).await;
}

/// Resize the terminal and PTY.
async fn handle_resize(
    session_id: SessionId,
    size: TerminalSize,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent Resize for unattached session");
        return;
    }

    if !size.has_grid() {
        warn!(%session_id, ?size, "ignoring resize with zero dimension");
        return;
    }

    let (term, resize_fd) = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            warn!(%session_id, "Resize for unknown session");
            return;
        };
        session.cell_width = size.cell_width.max(1);
        session.cell_height = size.cell_height.max(1);
        (Arc::clone(&session.term), Arc::clone(&session.resize_fd))
    };

    // Resize the Term state (lock + drop before any await).
    resize_term(&term, size.cols, size.rows).await;

    // Signal the PTY with TIOCSWINSZ.
    if let Err(e) = set_pty_winsize(resize_fd.as_ref(), size) {
        warn!(%session_id, "TIOCSWINSZ failed: {e}");
    }
}

async fn handle_search_request(
    session_id: SessionId,
    query: String,
    limit: u32,
    context: &ClientDispatchContext<'_>,
) {
    if !attached_contains(context.attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent SearchRequest for unattached session");
        return;
    }

    let sessions = context.server.live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        warn!(%session_id, "SearchRequest for unknown session");
        return;
    };
    let term = Arc::clone(&session.term);
    drop(sessions);

    let matches = {
        let term_guard = term.lock().await;
        let snapshot = snapshot_term(&term_guard);
        search_snapshot(&snapshot, &query, limit)
    };

    let msg = ServerMessage::SearchResults { session_id, query, matches };
    send_message(context.writer, &msg).await;
}

fn search_snapshot(snapshot: &ScreenSnapshot, query: &str, limit: u32) -> Vec<SearchMatch> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let needle: Vec<char> = query.chars().collect();
    if needle.is_empty() {
        return Vec::new();
    }

    let max_matches = limit as usize;
    let cols = usize::from(snapshot.cols);
    let history_rows = snapshot.scrollback_rows as usize;
    let history_rows_i32 = i32::try_from(history_rows).unwrap_or(i32::MAX);
    let mut matches = Vec::new();

    for row in 0..history_rows {
        if matches.len() >= max_matches {
            break;
        }

        let start = row.saturating_mul(cols);
        let end = start.saturating_add(cols);
        let row_i32 = i32::try_from(row).unwrap_or(i32::MAX);
        let absolute_row = -history_rows_i32 + row_i32;
        push_row_matches(
            snapshot.scrollback.get(start..end).unwrap_or(&[]),
            absolute_row,
            &needle,
            &mut matches,
            max_matches,
        );
    }

    for row in 0..usize::from(snapshot.rows) {
        if matches.len() >= max_matches {
            break;
        }

        let start = row.saturating_mul(cols);
        let end = start.saturating_add(cols);
        let row_i32 = i32::try_from(row).unwrap_or(i32::MAX);
        push_row_matches(
            snapshot.cells.get(start..end).unwrap_or(&[]),
            row_i32,
            &needle,
            &mut matches,
            max_matches,
        );
    }

    matches
}

fn push_row_matches(
    row_cells: &[ScreenCell],
    row: i32,
    needle: &[char],
    matches: &mut Vec<SearchMatch>,
    max_matches: usize,
) {
    if row_cells.is_empty() || needle.is_empty() || row_cells.len() < needle.len() {
        return;
    }

    let haystack: Vec<char> =
        row_cells.iter().map(|cell| if cell.c == '\0' { ' ' } else { cell.c }).collect();
    let last_start = haystack.len().saturating_sub(needle.len());

    for start in 0..=last_start {
        if haystack.get(start..start + needle.len()).is_some_and(|window| window == needle) {
            let Some(col_start) = u16::try_from(start).ok() else { break };
            let Some(col_end) = u16::try_from(start + needle.len() - 1).ok() else { break };
            matches.push(SearchMatch { row, col_start, col_end });
            if matches.len() >= max_matches {
                return;
            }
        }
    }
}

/// Terminal dimensions for `Term::resize()`.
struct ResizeDimensions {
    cols: usize,
    lines: usize,
}

impl alacritty_terminal::grid::Dimensions for ResizeDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Lock the `Term` and apply the new dimensions.
pub async fn resize_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    cols: u16,
    rows: u16,
) {
    let mut term_guard = term.lock().await;
    let size = ResizeDimensions { cols: usize::from(cols), lines: usize::from(rows) };
    term_guard.resize(size);
    // Guard dropped here — before any subsequent .await.
}

/// Set PTY window size via `TIOCSWINSZ` ioctl.
///
/// Writes a terminal `Winsize` to the PTY fd, which causes the kernel to send
/// `SIGWINCH` to the foreground process group.
pub fn set_pty_winsize(fd: impl AsFd, size: TerminalSize) -> Result<(), ScribeError> {
    let ws = rustix::termios::Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols.saturating_mul(size.cell_width.max(1)),
        ws_ypixel: size.rows.saturating_mul(size.cell_height.max(1)),
    };

    rustix::termios::tcsetwinsize(fd, ws).map_err(std::io::Error::from).map_err(ScribeError::from)
}

/// Handle `Subscribe` — trigger CWD fallback check for visible sessions.
async fn handle_subscribe(
    session_ids: &[SessionId],
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    let sessions = live_sessions.read().await;
    for &session_id in session_ids {
        if !attached_contains(attached_ids, session_id).await {
            warn!(%session_id, "Subscribe denied for unattached session");
            continue;
        }

        let Some(session) = sessions.get(&session_id) else {
            continue;
        };

        let msg = {
            let mut wm = workspace_manager.write().await;
            wm.check_cwd_fallback(session_id, session.child_pid)
        };

        if let Some(named_msg) = msg {
            send_message(writer, &named_msg).await;
        }
    }
}

/// Handle `RequestSnapshot` — snapshot the terminal and send it to the client.
async fn handle_request_snapshot(
    session_id: SessionId,
    writer: &SharedWriter,
    live_sessions: &LiveSessionRegistry,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        send_error(writer, &format!("RequestSnapshot denied for unattached session {session_id}"))
            .await;
        return;
    }

    let sessions = live_sessions.read().await;
    let Some(session) = sessions.get(&session_id) else {
        send_error(writer, &format!("RequestSnapshot for unknown session {session_id}")).await;
        return;
    };

    let term = session.term.lock().await;
    let snapshot = snapshot_term(&term);
    drop(term);
    drop(sessions);

    let msg = ServerMessage::ScreenSnapshot { session_id, snapshot };
    send_message(writer, &msg).await;
}

/// Handle `CreateWorkspace` — create a new workspace and send info to the client.
async fn handle_create_workspace(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) {
    let mut wm = workspace_manager.write().await;
    let workspace_id = wm.create_workspace();
    let (name, accent_color, split_direction, project_root) = wm
        .workspace_info(workspace_id)
        .unwrap_or_else(|| (None, String::from("#a78bfa"), None, None));
    drop(wm);

    let msg = ServerMessage::WorkspaceInfo {
        workspace_id,
        name,
        accent_color,
        split_direction,
        project_root,
    };
    send_message(writer, &msg).await;
}

/// Handle `ListSessions` — reply with all live sessions and their workspace info.
async fn handle_list_sessions(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    window_id: WindowId,
) {
    let sessions = live_sessions.read().await;
    let wm = workspace_manager.read().await;

    // Filter sessions to those belonging to this window.
    let window_session_ids = wm.sessions_for_window(window_id);
    let has_window_sessions = !window_session_ids.is_empty();

    let build_info = |sid: SessionId, s: &LiveSession| SessionInfo {
        session_id: sid,
        workspace_id: s.workspace_id,
        shell_name: s.shell_name.clone(),
        title: Some(s.title.clone()),
        context: s.context.clone(),
        task_label: s.task_label.clone(),
        codex_task_label: s.task_label.clone(),
        cwd: s.cwd.clone(),
        git_branch: s.cwd.as_deref().and_then(detect_git_branch),
        ai_state: s.ai_state.clone(),
        ai_provider_hint: s.ai_state.as_ref().map(|state| state.provider).or(s.ai_provider_hint),
    };

    let infos: Vec<SessionInfo> = if has_window_sessions {
        // Return only this window's sessions.
        window_session_ids
            .iter()
            .filter_map(|&sid| sessions.get(&sid).map(|s| build_info(sid, s)))
            .collect()
    } else {
        // No window-specific sessions — return all unowned sessions (legacy
        // fallback or first-time connect with existing sessions).
        sessions
            .iter()
            .filter(|&(&sid, _)| wm.window_for_session(sid).is_none())
            .map(|(&id, s)| build_info(id, s))
            .collect()
    };

    // Batch per-workspace metadata into the SessionList so clients do not need
    // a separate per-session WorkspaceInfo fan-out during reattach.
    let mut seen = HashSet::new();
    let mut workspaces: Vec<WorkspaceListEntry> = Vec::new();
    for info in &infos {
        if !seen.insert(info.workspace_id) {
            continue;
        }
        if let Some((name, accent_color, split_direction, project_root)) =
            wm.workspace_info(info.workspace_id)
        {
            workspaces.push(WorkspaceListEntry {
                workspace_id: info.workspace_id,
                name,
                accent_color,
                split_direction,
                project_root,
            });
        }
    }
    let workspace_tree = wm.window_tree(window_id).cloned();
    drop(wm);
    drop(sessions);

    let list_msg = ServerMessage::SessionList { sessions: infos, workspace_tree, workspaces };
    send_message(writer, &list_msg).await;
}

async fn handle_workspace_notes_get(
    workspace_notes: &Arc<Mutex<WorkspaceNotesStore>>,
    writer: &SharedWriter,
    workspace_ids: &[WorkspaceId],
) {
    let collections = workspace_notes.lock().await.collections_for(workspace_ids);
    send_message(writer, &ServerMessage::WorkspaceNotesSnapshot { collections }).await;
}

async fn handle_workspace_notes_mutate(
    workspace_notes: &Arc<Mutex<WorkspaceNotesStore>>,
    connected_clients: &ConnectedClients,
    writer: &SharedWriter,
    mutation: WorkspaceNotesMutation,
) {
    let collection = match workspace_notes.lock().await.apply_mutation(mutation) {
        Ok(collection) => collection,
        Err(error) => {
            send_error(writer, &format!("workspace note mutation failed: {error}")).await;
            return;
        }
    };
    broadcast_workspace_notes_changed(connected_clients, collection).await;
}

async fn broadcast_workspace_notes_changed(
    connected_clients: &ConnectedClients,
    collection: scribe_common::protocol::WorkspaceNotesCollection,
) {
    let msg = ServerMessage::WorkspaceNotesChanged { collection };
    let clients = connected_clients.read().await;
    for writer in clients.values() {
        send_message(writer, &msg).await;
    }
}

/// Handle `AttachSessions` — take ownership of detached sessions, set the
/// client writer, and send back session + workspace info for each.
async fn handle_attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    context: &mut ClientDispatchContext<'_>,
) {
    let (session_ids, dimensions) =
        filter_attachable_sessions(session_ids, dimensions, context).await;
    if session_ids.is_empty() {
        return;
    }

    let attached = crate::attach_flow::attach_sessions(
        &session_ids,
        &dimensions,
        &context.server.live_sessions,
        crate::attach_flow::AttachClientContext {
            writer: context.writer,
            attached_ids: context.attached_ids,
        },
    )
    .await;
    attached_extend(context.attached_ids, attached).await;
}

async fn filter_attachable_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    context: &ClientDispatchContext<'_>,
) -> (Vec<SessionId>, Vec<TerminalSize>) {
    // Feature 013 takeover authorization: `AttachSessions` re-points each session's
    // client writer, so a connection displaced by a takeover could otherwise
    // re-steal the `PtyOutput` stream (and clipboard-bridge routing) by simply
    // re-sending it — its `attached_ids` and static workspace membership still
    // match. Deny the whole batch unless this connection is STILL the window's
    // registered controller. Local clients always are (no-op for the local path).
    if !connection_controls_window(
        &context.server.connected_clients,
        context.window_id,
        context.writer,
    )
    .await
    {
        debug!(
            window_id = %context.window_id,
            "AttachSessions denied: connection is not the window's current controller"
        );
        return (Vec::new(), Vec::new());
    }

    let wm = context.server.workspace_manager.read().await;
    let include_dimensions = !dimensions.is_empty();
    let mut allowed_ids = Vec::with_capacity(session_ids.len());
    let mut allowed_dimensions = Vec::with_capacity(dimensions.len().min(session_ids.len()));
    let mut denied_ids = Vec::new();

    for (idx, &session_id) in session_ids.iter().enumerate() {
        match wm.window_for_session(session_id) {
            Some(owner) if owner != context.window_id => {
                warn!(%session_id, owner = %owner, requester = %context.window_id, "AttachSessions denied for another window's session");
                denied_ids.push(session_id);
            }
            _ => {
                allowed_ids.push(session_id);
                if include_dimensions {
                    allowed_dimensions.push(dimensions.get(idx).copied().unwrap_or_default());
                }
            }
        }
    }
    drop(wm);

    for session_id in denied_ids {
        send_error(context.writer, &format!("AttachSessions denied for session {session_id}"))
            .await;
    }

    (allowed_ids, allowed_dimensions)
}

/// Handle `ConfigReloaded` — reload the config file and apply live changes.
async fn handle_config_reloaded(server: &IpcServerState) {
    let session_manager = &server.session_manager;
    let workspace_manager = &server.workspace_manager;
    let live_sessions = &server.live_sessions;
    let env_store = &server.env_store;

    let cfg = match crate::config::load_config() {
        Ok(cfg) => {
            info!("config reloaded successfully via client request");
            cfg
        }
        Err(e) => {
            warn!("config reload failed: {e}");
            return;
        }
    };

    let new_scrollback = usize::try_from(cfg.scrollback_lines).unwrap_or(usize::MAX);
    session_manager.set_scrollback_lines(new_scrollback);

    let term_config = build_term_config(new_scrollback);
    let sessions = live_sessions.read().await;
    let mut workspace_messages = Vec::new();
    {
        let mut wm = workspace_manager.write().await;
        wm.set_roots(cfg.workspace_roots.clone());
        for (&session_id, session) in sessions.iter() {
            let named_msg = if let Some(cwd) = session.cwd.as_deref() {
                wm.on_cwd_changed(session_id, cwd)
            } else {
                wm.check_cwd_fallback(session_id, session.child_pid)
            };
            if let Some(msg) = named_msg {
                workspace_messages.push((Arc::clone(&session.client_writer), msg));
            }
        }
    }
    apply_reload_to_sessions(&sessions, &term_config, new_scrollback, &cfg).await;
    let sessions_len = sessions.len();
    drop(sessions);

    for (client_writer, msg) in workspace_messages {
        send_to_client(&client_writer, &msg).await;
    }
    info!(
        scrollback_lines = new_scrollback,
        preserve_ai_scrollback = cfg.ai_terminal.preserve_ai_scrollback,
        sessions = sessions_len,
        "config reload applied to live sessions"
    );

    apply_env_persistence_transition(env_store, live_sessions).await;

    // Feature 013 (T007): poke the remote-control supervisor so it re-reads the
    // reloaded `[remote]` config and starts, stops, or rebinds the listener live.
    // Synchronous notify — the actual apply runs on the supervisor task, never on
    // this dispatch loop, and the server is never restarted.
    server.remote_control.request_reload();
}

/// Apply a reloaded server config to every live session: refresh the
/// alacritty `TermConfig`, the scrollback cap, the AI-scrollback flag, and
/// the OSC 52 `ClipboardPolicyConfig` snapshot (FR-010). Extracted from
/// [`handle_config_reloaded`] so the parent stays below the cognitive-
/// complexity budget while keeping the per-session fan-out in one place.
async fn apply_reload_to_sessions(
    sessions: &HashMap<SessionId, LiveSession>,
    term_config: &alacritty_terminal::term::Config,
    new_scrollback: usize,
    cfg: &crate::config::ScribeConfig,
) {
    let clipboard_policy = cfg.clipboard_policy.clone();
    for (session_id, session) in sessions {
        session.term.lock().await.set_options(term_config.clone());
        session.scrollback_lines.store(new_scrollback, Ordering::Relaxed);
        session
            .preserve_ai_scrollback
            .store(cfg.ai_terminal.preserve_ai_scrollback, Ordering::Relaxed);
        if session
            .clipboard_command_tx
            .send(ClipboardCommand::RefreshPolicy { policy: clipboard_policy.clone() })
            .is_err()
        {
            debug!(
                %session_id,
                "clipboard policy refresh dropped: command channel closed"
            );
        }
    }
}

/// T035: react to a `terminal.env_persistence.enabled` flip across a
/// `ConfigReloaded`.
///
/// On `false → true`: no proactive action — `hook_ingress` already
/// lazy-initializes per-session `env_store` machinery on the next
/// baseline-ready `EnvChanged`. Just emit a marker log.
///
/// On `true → false`: stop every per-session persist timer (via
/// [`EnvStoreState::drop_scheduler`]) and best-effort delete every
/// envelope under `restore/env/<window_id>/` for every distinct
/// `env_window_id` in the live-session registry (via
/// [`env_store::store::delete_window_envelopes`]). Failures are logged at
/// warn and do not abort the reload — per FR-009 + R4.6 the disable
/// transition is an explicit user action that fully discards on-disk
/// state, but a partial-delete failure must not poison the rest of the
/// reload path.
///
/// Reads the freshly-on-disk feature flag via
/// `scribe_common::config::load_config()` and atomically swaps it into
/// [`EnvStoreState`]'s cached `last_enabled`. No-op when the flag did
/// not change.
async fn apply_env_persistence_transition(
    env_store: &Arc<crate::env_store::EnvStoreState>,
    live_sessions: &LiveSessionRegistry,
) {
    let new_enabled = match scribe_config::load_config() {
        Ok(cfg) => cfg.terminal.env_persistence.enabled,
        Err(e) => {
            warn!(
                target: "scribe_server::ipc_server",
                error = %e,
                "skipping env-persistence transition check: config load failed"
            );
            return;
        }
    };
    let old_enabled = env_store.swap_last_enabled(new_enabled);

    if old_enabled == new_enabled {
        return;
    }

    if old_enabled && !new_enabled {
        // Disable transition: stop schedulers across all sessions and
        // best-effort delete every envelope under each distinct
        // `env_window_id`. Snapshot session_ids + window_ids under the
        // read lock, then drop the lock before doing any async work that
        // could await for a non-trivial time.
        let (session_ids, window_ids): (Vec<SessionId>, HashSet<WindowId>) = {
            let sessions = live_sessions.read().await;
            let ids: Vec<SessionId> = sessions.keys().copied().collect();
            let wids: HashSet<WindowId> =
                sessions.values().map(|live| live.env_window_id).collect();
            (ids, wids)
        };

        for sid in &session_ids {
            env_store.drop_scheduler(*sid).await;
        }

        for wid in &window_ids {
            if let Err(e) = crate::env_store::store::delete_window_envelopes(*wid).await {
                warn!(
                    target: "scribe_server::ipc_server",
                    error = ?e,
                    window_id = %wid,
                    "delete_window_envelopes failed on env-persistence disable transition"
                );
            }
        }

        info!(
            target: "scribe_server::ipc_server",
            sessions = session_ids.len(),
            windows = window_ids.len(),
            "env persistence disabled; schedulers dropped and envelopes deleted"
        );
    } else {
        info!(
            target: "scribe_server::ipc_server",
            "env persistence enabled; per-session machinery will initialize on next EnvChanged event"
        );
    }
}

fn load_preserve_ai_scrollback_setting() -> bool {
    match scribe_common::config::load_config() {
        Ok(config) => config.terminal.ai_session.preserve_ai_scrollback,
        Err(e) => {
            warn!("failed to load preserve_ai_scrollback setting: {e}");
            true
        }
    }
}

fn load_scrollback_lines_setting() -> usize {
    match scribe_common::config::load_config() {
        Ok(config) => usize::try_from(config.terminal.scrollback_lines).unwrap_or(usize::MAX),
        Err(e) => {
            warn!("failed to load scrollback_lines setting: {e}");
            10_000
        }
    }
}

/// Broadcast `QuitRequested` to all connected clients, including the sender.
///
/// Per T019, also sweeps every env envelope under every live window before
/// telling clients to shut down. Clients will follow up with their own
/// `CloseWindow` messages, but a per-window pre-sweep here protects against
/// clients that fail to ack `QuitRequested` (crash, race, transport drop) —
/// without it, those envelopes would survive across the quit. The
/// `delete_window_envelopes` path is idempotent, so the subsequent
/// `CloseWindow` sweeps are no-ops if they still arrive.
///
/// Window enumeration unions `connected_clients` keys and
/// `workspace_manager::window_ids_with_sessions` (same merge
/// `handle_list_windows` uses) so disconnected windows that still own live
/// sessions are not skipped.
async fn handle_quit_all(
    sender_window_id: WindowId,
    connected_clients: &ConnectedClients,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    info!(%sender_window_id, "QuitAll requested — broadcasting QuitRequested");

    // Compute the union of live windows before any client teardown can
    // mutate workspace state. Read locks here only — no async work under
    // them — so the order matches `handle_list_windows`.
    let window_ids: HashSet<WindowId> = {
        let clients = connected_clients.read().await;
        let wm = workspace_manager.read().await;
        let mut ids: HashSet<WindowId> = wm.window_ids_with_sessions();
        ids.extend(clients.keys().copied());
        ids
    };

    // Best-effort per-window envelope sweep. Done before broadcasting
    // `QuitRequested` so the deletes are not racing client-driven
    // `CloseWindow` traffic.
    for window_id in &window_ids {
        if let Err(err) = crate::env_store::store::delete_window_envelopes(*window_id).await {
            warn!(
                target: "scribe_server::ipc_server",
                %window_id,
                error = ?err,
                "env-envelope window sweep failed during QuitAll"
            );
        }
    }

    let clients = connected_clients.read().await;
    let quit_msg = ServerMessage::QuitRequested;
    for writer in clients.values() {
        send_message(writer, &quit_msg).await;
    }
}

async fn handle_list_windows(
    connected_clients: &ConnectedClients,
    window_controllers: &WindowControllers,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) {
    let connected = connected_clients.read().await;
    let controllers = window_controllers.read().await;
    let wm = workspace_manager.read().await;

    let mut window_ids: HashSet<WindowId> = wm.window_ids_with_sessions();
    window_ids.extend(connected.keys().copied());

    // Feature 013: enrich each entry with the picker/indicator context —
    // workspace names for the window list (FR-005) and the current remote
    // controller's device + account when the window is remote-controlled
    // (FR-009b / SC-006). A locally-controlled or unconnected window carries
    // `controller = None`.
    let mut windows: Vec<WindowInfo> = window_ids
        .into_iter()
        .map(|window_id| WindowInfo {
            window_id,
            session_count: wm.sessions_for_window(window_id).len(),
            connected: connected.contains_key(&window_id),
            workspace_names: wm.workspace_names_for_window(window_id),
            controller: controllers
                .get(&window_id)
                .and_then(ControllerIdentity::to_controller_info),
        })
        .collect();
    windows.sort_by_key(|info| info.window_id.to_full_string());
    drop(wm);
    drop(controllers);
    drop(connected);

    send_message(writer, &ServerMessage::WindowList { windows }).await;
}

async fn handle_dispatch_action(
    requested_window_id: Option<WindowId>,
    action: AutomationAction,
    connected_clients: &ConnectedClients,
    sender_window_id: WindowId,
    writer: &SharedWriter,
) {
    if let Some(window_id) = requested_window_id {
        if window_id != sender_window_id {
            send_error(writer, &format!("cannot dispatch action to another window: {window_id}"))
                .await;
            return;
        }
    }

    let connected = connected_clients.read().await;
    let requested_window_id = requested_window_id.unwrap_or(sender_window_id);
    let target_window_id =
        connected.contains_key(&requested_window_id).then_some(requested_window_id);

    let target_writer = target_window_id.and_then(|window_id| connected.get(&window_id).cloned());
    drop(connected);

    let Some(target_window_id) = target_window_id else {
        send_error(writer, &format!("window not connected: {requested_window_id}")).await;
        return;
    };
    let Some(target_writer) = target_writer else {
        send_error(writer, &format!("window not connected: {target_window_id}")).await;
        return;
    };

    if !try_send_message(&target_writer, &ServerMessage::RunAction { action }).await {
        send_error(writer, &format!("failed to dispatch action to {target_window_id}")).await;
        return;
    }

    send_message(writer, &ServerMessage::ActionDispatched { window_id: target_window_id }).await;
}

/// Run the OS keystore preflight probe on behalf of the settings UI and
/// reply with `ServerMessage::EnvPreflightResult`.
///
/// `EnvPreflight` is a user-triggered, infrequent request (toggle in
/// Settings → Terminal → General). The handler awaits the keystore probe
/// inline — `keystore::preflight()` already wraps the synchronous keyring
/// calls in `spawn_blocking`, so the dispatch loop is not stalled. No
/// retry, throttle, or rate limit is applied here per design.
async fn handle_env_preflight(writer: &SharedWriter) {
    let result = match crate::env_store::keystore::preflight().await {
        Ok(()) => ServerMessage::EnvPreflightResult { ok: true, error: None },
        Err(e) => {
            tracing::warn!(
                target: "scribe_server::ipc_server",
                error = ?e,
                "env preflight failed"
            );
            ServerMessage::EnvPreflightResult {
                ok: false,
                error: Some(crate::env_store::keystore::to_preflight_error(&e)),
            }
        }
    };
    send_message(writer, &result).await;
}

/// Send a `ServerMessage` to the client, logging errors.
pub async fn send_message(writer: &SharedWriter, msg: &ServerMessage) {
    let _ = try_send_message(writer, msg).await;
}

async fn try_send_message(writer: &SharedWriter, msg: &ServerMessage) -> bool {
    let mut w = writer.lock().await;
    write_to_sink(&mut w, msg).await
}

/// Write (local) or enqueue (remote) one `ServerMessage` on a client sink. A
/// local sink writes the framed message straight to its socket — unchanged
/// pre-013 behavior. A remote sink hands it to the bounded per-connection output
/// queue (T029), which never blocks on the link; overflow is absorbed inside the
/// queue, so the enqueue arm always reports success. Returns `false` only on a
/// local write error (a dead socket).
async fn write_to_sink(sink: &mut ClientSink, msg: &ServerMessage) -> bool {
    match sink {
        ClientSink::Local(w) => match write_message(w, msg).await {
            Ok(()) => true,
            Err(e) => {
                warn!("failed to send message to client: {e}");
                false
            }
        },
        ClientSink::Remote(remote) => {
            remote.enqueue(msg);
            true
        }
    }
}

/// Send a `ServerMessage` via the optional client writer. No-op when the
/// session is detached (writer is `None`).
async fn send_to_client(client_writer: &ClientWriter, msg: &ServerMessage) {
    let guard = client_writer.lock().await;
    if let Some(writer) = guard.as_ref() {
        let mut w = writer.lock().await;
        write_to_sink(&mut w, msg).await;
    }
}

/// Send an error message to the client.
async fn send_error(writer: &SharedWriter, message: &str) {
    let msg = ServerMessage::Error { message: message.to_owned() };
    send_message(writer, &msg).await;
}

// ── PTY reader task ─────────────────────────────────────────────

/// The dual-path read loop: raw bytes to UI (fast path) + Term state + metadata.
///
/// Uses `ClientWriter` (optional) so the task keeps running even when no
/// client is connected. Output is silently dropped when detached, but the
/// `Term` state continues to be updated.
async fn pty_reader_task(mut state: PtyReaderState) {
    let mut buf = vec![0u8; PTY_READ_BUF_SIZE];

    loop {
        match next_pty_read_action(&mut state, &mut buf).await {
            PtyReadAction::Continue => {}
            PtyReadAction::End => break,
            PtyReadAction::Data(bytes_read) => {
                let Some(bytes) = buf.get(..bytes_read) else { break };
                process_pty_chunk(&mut state, bytes).await;
            }
        }
    }

    finalize_pty_reader(state).await;
}

enum PtyReadAction {
    Continue,
    Data(usize),
    End,
}

async fn next_pty_read_action(state: &mut PtyReaderState, buf: &mut [u8]) -> PtyReadAction {
    let Some(read_result) = select_pty_read_or_clipboard(state, buf).await else {
        return PtyReadAction::Continue;
    };

    match read_result {
        ReadResult::Data(n) => PtyReadAction::Data(n),
        ReadResult::Eof => PtyReadAction::End,
        ReadResult::Err(e) => {
            warn!(session_id = %state.session_id, "PTY read error: {e}");
            PtyReadAction::End
        }
    }
}

/// Race a PTY read against the optional ANSI sync-timeout sleep and the
/// OSC 52 [`ClipboardCommand`] channel. Returns `Some(read_result)` when
/// the PTY produced bytes (or an error/EOF), or `None` when either the
/// sync timeout fired or a clipboard command was consumed — both of those
/// paths are "continue the outer loop" signals for [`next_pty_read_action`].
async fn select_pty_read_or_clipboard(
    state: &mut PtyReaderState,
    buf: &mut [u8],
) -> Option<ReadResult> {
    if let Some(deadline) = state.ansi_processor.sync_timeout().sync_timeout() {
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => {
                stop_term_sync(&state.term, &mut state.ansi_processor).await;
                maybe_capture_preserved_ai_scrollback_baseline(state).await;
                None
            }
            result = read_pty_bytes(&mut state.pty_read, buf) => Some(result),
            cmd = state.clipboard_command_rx.recv() => {
                handle_clipboard_command_option(state, cmd).await;
                None
            }
        }
    } else {
        tokio::select! {
            result = read_pty_bytes(&mut state.pty_read, buf) => Some(result),
            cmd = state.clipboard_command_rx.recv() => {
                handle_clipboard_command_option(state, cmd).await;
                None
            }
        }
    }
}

/// Apply one `ClipboardCommand` (or no-op on a `None` from a closed
/// channel). Kept separate so `next_pty_read_action` stays an `async fn`
/// without polluting the macro selectors with `Option` destructuring.
async fn handle_clipboard_command_option(
    state: &mut PtyReaderState,
    cmd: Option<ClipboardCommand>,
) {
    let Some(cmd) = cmd else { return };
    match cmd {
        ClipboardCommand::PromptResponse { request_id, decision } => {
            handle_clipboard_prompt_response(state, request_id, decision).await;
        }
        ClipboardCommand::BridgeReadReply { request_id, payload } => {
            handle_clipboard_bridge_read_reply(state, request_id, payload).await;
        }
        ClipboardCommand::RefreshPolicy { policy } => {
            handle_clipboard_policy_refresh(state, policy);
        }
    }
}

/// Spec 010 T036: replace the per-session policy snapshot in place when
/// `ConfigReloaded` broadcasts a fresh `terminal.clipboard.*` set. Leaves
/// `outstanding_prompt` untouched so an in-flight prompt still resolves
/// against the operation it was opened for; subsequent OSC 52 ops use the
/// new policy.
fn handle_clipboard_policy_refresh(
    state: &mut PtyReaderState,
    policy: scribe_common::config::ClipboardPolicyConfig,
) {
    debug!(
        session_id = %state.session_id,
        read_mode = ?policy.read_mode,
        write_mode = ?policy.write_mode,
        max_write_bytes = policy.max_write_bytes,
        "applying OSC 52 policy refresh to live session"
    );
    state.clipboard_burst.policy = policy;
}

async fn process_pty_chunk(state: &mut PtyReaderState, bytes: &[u8]) {
    capture_osc_metadata_events(state, bytes);
    prepare_preserved_ai_scrollback_epoch(state);
    let effective = apply_pty_filters(state, bytes);
    let suppressed_ed3 = state.ed3_filter.take_suppressed();
    let capture_baseline_after_feed =
        suppressed_ed3 && state.preserved_ai_scrollback.needs_baseline();
    let trimmed_rows = if suppressed_ed3 { handle_suppressed_ai_ed3(state).await } else { None };

    if let Some(rows) = trimmed_rows {
        send_trim_scrollback(&state.client_writer, state.session_id, rows).await;
    }

    // Step 1: Fast path — forward (possibly filtered) bytes to UI client.
    send_pty_output(&state.client_writer, state.session_id, effective.as_ref()).await;

    // Step 1b: If ED 3 was suppressed, tell the client to snap the
    // viewport to bottom.  A real ED 3 would have reset `display_offset`
    // to 0 inside `clear_history()`, but since we stripped the sequence,
    // the client's Term never ran that code.
    if suppressed_ed3 {
        let msg = ServerMessage::ScrollBottom { session_id: state.session_id };
        send_to_client(&state.client_writer, &msg).await;
    }

    // Step 2: State path — feed (possibly filtered) bytes into Term.
    if capture_baseline_after_feed {
        state.pending_ai_scrollback_baseline = true;
    }
    feed_term(&state.term, &mut state.ansi_processor, effective.as_ref()).await;
    maybe_capture_preserved_ai_scrollback_baseline(state).await;

    // Step 2b: a prompt returning while mouse-reporting modes are still
    // active means the foreground program died without cleanup (e.g. a
    // force-closed SSH session whose remote TUI never sent DECRST) —
    // inject the reset so client Terms, this Term, and future replay
    // snapshots all clear together.
    clear_stale_mouse_modes_at_prompt(state).await;

    // Steps 3–5: Metadata uses original bytes (OSC parser doesn't care about CSI ED 3).
    process_metadata_events(state).await;
}

/// Reset mouse-reporting / focus-event modes left enabled when a shell
/// prompt returns (OSC 133 `A`), i.e. the foreground program exited or
/// died without cleaning up. Stale modes turn every mouse movement into
/// `\x1b[<…M` reports that the shell merely echoes as garbage until the
/// user runs `reset`; because the modes also live in this Term they would
/// otherwise even survive reattach via `SessionReplay`.
///
/// The DECRST bytes are injected into both consumers of the output
/// stream — the server Term (mode source for replay snapshots) and the
/// client-bound `PtyOutput` path (every attached client's Term parses
/// them and stops forwarding mouse events).
async fn clear_stale_mouse_modes_at_prompt(state: &mut PtyReaderState) {
    if !prompt_returned_without_new_command(&state.osc_events) {
        return;
    }
    let resets = {
        let term_guard = state.term.lock().await;
        stale_mouse_mode_resets(*term_guard.mode())
    };
    let Some(resets) = resets else { return };
    debug!(
        session_id = %state.session_id,
        "prompt returned with mouse-reporting modes active; injecting DECRST"
    );
    feed_term(&state.term, &mut state.ansi_processor, &resets).await;
    send_pty_output(&state.client_writer, state.session_id, &resets).await;
}

/// `true` when the chunk contains a shell `PromptStart` mark with no
/// `CommandStart` after it.
///
/// The trailing-`CommandStart` guard covers type-ahead: when the user has
/// already typed the next command before the prompt rendered, one PTY
/// chunk can carry `133;A … 133;C` plus the new program's own DECSET —
/// resetting then would break the program that just legitimately enabled
/// mouse reporting.
fn prompt_returned_without_new_command(events: &[MetadataEvent]) -> bool {
    let last_prompt_start = events.iter().rposition(|event| {
        matches!(event, MetadataEvent::PromptMark { kind: PromptMarkKind::PromptStart, .. })
    });
    let Some(idx) = last_prompt_start else {
        return false;
    };
    !events.get(idx..).unwrap_or(&[]).iter().any(|event| {
        matches!(event, MetadataEvent::PromptMark { kind: PromptMarkKind::CommandStart, .. })
    })
}

/// Build the DECRST byte sequence clearing whichever mouse-reporting and
/// focus-event modes are active, or `None` when none are.
///
/// Scope is exactly the garbage-producing set: the mouse protocols
/// (1000/1002/1003), their encodings (1005/1006), and focus events
/// (1004). Bracketed paste and application cursor/keypad are deliberately
/// untouched — shells legitimately manage those across prompts.
fn stale_mouse_mode_resets(mode: alacritty_terminal::term::TermMode) -> Option<Vec<u8>> {
    use alacritty_terminal::term::TermMode;

    const MODES: &[(TermMode, u16)] = &[
        (TermMode::MOUSE_REPORT_CLICK, 1000),
        (TermMode::MOUSE_DRAG, 1002),
        (TermMode::MOUSE_MOTION, 1003),
        (TermMode::FOCUS_IN_OUT, 1004),
        (TermMode::UTF8_MOUSE, 1005),
        (TermMode::SGR_MOUSE, 1006),
    ];

    // Fire only when a report-generating mode is on; a lingering encoding
    // bit (1005/1006) alone emits nothing and is not worth a reset.
    if !mode.intersects(TermMode::MOUSE_MODE.union(TermMode::FOCUS_IN_OUT)) {
        return None;
    }

    let mut bytes = Vec::new();
    for (bit, param) in MODES {
        if mode.contains(*bit) {
            bytes.extend_from_slice(format!("\x1b[?{param}l").as_bytes());
        }
    }
    (!bytes.is_empty()).then_some(bytes)
}

fn apply_pty_filters<'a>(state: &mut PtyReaderState, bytes: &'a [u8]) -> Cow<'a, [u8]> {
    let chunk_has_ed3_provider = chunk_mentions_ed3_provider(&state.osc_events);
    let preserve = state.preserve_ai_scrollback.load(Ordering::Relaxed);
    if !preserve {
        reset_preserved_ai_scrollback_epoch(state);
    }
    let ed3_output =
        if preserve && should_apply_ed3_filter(state.ai_provider, chunk_has_ed3_provider) {
            state.ed3_filter.filter(bytes)
        } else {
            scribe_pty::ed3_filter::Ed3Output::Unchanged(bytes)
        };
    let after_ed3 = match ed3_output {
        scribe_pty::ed3_filter::Ed3Output::Unchanged(filtered_bytes) => {
            Cow::Borrowed(filtered_bytes)
        }
        scribe_pty::ed3_filter::Ed3Output::Filtered(filtered_bytes) => Cow::Owned(filtered_bytes),
    };

    let after_claude_picker = if ai_provider_uses_claude_picker_filter(state.ai_provider) {
        match state.claude_picker_filter.filter(after_ed3.as_ref()) {
            scribe_pty::claude_picker_filter::ClaudePickerOutput::Unchanged(_) => after_ed3,
            scribe_pty::claude_picker_filter::ClaudePickerOutput::Filtered(filtered_bytes) => {
                Cow::Owned(filtered_bytes)
            }
        }
    } else {
        after_ed3
    };

    // Always-on workaround for alacritty_terminal 0.26.0's `linefeed` bug:
    // bare LF after a print-at-last-column with DECAWM advances the cursor
    // by 2 rows instead of 1 because `input_needs_wrap` is not cleared.
    // Inserting `\r` before any bare LF makes the parser run
    // `carriage_return` first, which clears the deferred-wrap flag.
    match state.lf_crlf_filter.filter(after_claude_picker.as_ref()) {
        scribe_pty::lf_crlf_filter::LfCrlfOutput::Unchanged(_) => after_claude_picker,
        scribe_pty::lf_crlf_filter::LfCrlfOutput::Filtered(filtered_bytes) => {
            Cow::Owned(filtered_bytes)
        }
    }
}

async fn handle_suppressed_ai_ed3(state: &mut PtyReaderState) -> Option<usize> {
    let current_history = {
        let term_guard = state.term.lock().await;
        term_guard.grid().history_size()
    };
    let trim_rows = state.preserved_ai_scrollback.trim_target(current_history);
    if let Some(kept_rows) = trim_rows {
        trim_term_scrollback(
            &state.term,
            kept_rows,
            state.scrollback_lines.load(Ordering::Relaxed),
        )
        .await;
    }
    trim_rows
}

async fn maybe_capture_preserved_ai_scrollback_baseline(state: &mut PtyReaderState) {
    if !state.pending_ai_scrollback_baseline || state.ansi_processor.sync_bytes_count() != 0 {
        return;
    }

    let history = {
        let term_guard = state.term.lock().await;
        term_guard.grid().history_size()
    };
    state.preserved_ai_scrollback.set_baseline(history);
    state.pending_ai_scrollback_baseline = false;
}

async fn trim_term_scrollback(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    kept_rows: usize,
    max_rows: usize,
) {
    let mut term_guard = term.lock().await;
    trim_term_scrollback_inner(&mut term_guard, kept_rows, max_rows);
}

fn trim_term_scrollback_inner(
    term: &mut alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>,
    kept_rows: usize,
    max_rows: usize,
) {
    let kept_rows = kept_rows.min(max_rows);
    let grid = term.grid_mut();
    grid.update_history(kept_rows);
    grid.update_history(max_rows);
}

async fn finalize_pty_reader(state: PtyReaderState) {
    let exit_msg = ServerMessage::SessionExited { session_id: state.session_id, exit_code: None };
    send_to_client(&state.client_writer, &exit_msg).await;
    remove_from_session_attachment(&state.attachment, state.session_id).await;
    state.live_sessions.write().await.remove(&state.session_id);
    let mut workspace_manager = state.workspace_manager.write().await;
    workspace_manager.remove_session(state.session_id);
    workspace_manager.remove_session_from_window(state.session_id);
    info!(session_id = %state.session_id, "PTY reader task exited");
}

/// Result of a PTY read attempt.
enum ReadResult {
    Data(usize),
    Eof,
    Err(std::io::Error),
}

/// Read bytes from the PTY read half.
async fn read_pty_bytes(
    pty_read: &mut ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    buf: &mut [u8],
) -> ReadResult {
    use tokio::io::AsyncReadExt as _;

    match pty_read.read(buf).await {
        Ok(0) => ReadResult::Eof,
        Ok(n) => ReadResult::Data(n),
        Err(e) => ReadResult::Err(e),
    }
}

/// Send raw PTY output to the client (fast path). No-op when detached.
async fn send_pty_output(client_writer: &ClientWriter, session_id: SessionId, bytes: &[u8]) {
    let msg = ServerMessage::PtyOutput { session_id, data: bytes.to_vec() };
    send_to_client(client_writer, &msg).await;
}

async fn send_trim_scrollback(
    client_writer: &ClientWriter,
    session_id: SessionId,
    history_rows: usize,
) {
    let msg = ServerMessage::TrimScrollback {
        session_id,
        history_rows: u32::try_from(history_rows).unwrap_or(u32::MAX),
    };
    send_to_client(client_writer, &msg).await;
}

/// Feed bytes into the terminal emulator via the ANSI processor.
/// The Term mutex lock is held only during `advance()` — dropped before returning.
async fn feed_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
) {
    let mut term_guard = term.lock().await;
    ansi_processor.advance(&mut *term_guard, bytes);
    // Guard dropped here — before any subsequent .await.
}

/// Flush a synchronized update after its timeout elapses.
async fn stop_term_sync(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    ansi_processor: &mut AnsiProcessor,
) {
    let mut term_guard = term.lock().await;
    ansi_processor.stop_sync(&mut *term_guard);
}

fn capture_osc_metadata_events(state: &mut PtyReaderState, bytes: &[u8]) {
    run_osc_interceptor(&mut state.osc_parser, bytes, &mut state.osc_events);
}

/// Parse OSC sequences from bytes using the interceptor. Pure computation, no async.
///
/// Events are pushed into `out`, which the caller clears between iterations to
/// avoid allocating a new `Vec` on every PTY read.
fn run_osc_interceptor(osc_parser: &mut VteParser, bytes: &[u8], out: &mut Vec<MetadataEvent>) {
    let mut interceptor = OscInterceptor::new(out);
    osc_parser.advance(&mut interceptor, bytes);
}

/// Run the OSC interceptor, drain the metadata channel, classify events,
/// and — if a title changed but no OSC 7 arrived — fall back to
/// `/proc/pid/cwd` for CWD detection.
async fn process_metadata_events(state: &mut PtyReaderState) {
    let mut saw_title_change = false;
    let mut saw_cwd_change = false;

    // OSC interceptor events captured before the UI fast path.
    let mut events_this_iter = std::mem::take(&mut state.osc_events);
    for event in events_this_iter.drain(..) {
        handle_session_event(
            SessionEvent::Metadata(event),
            state,
            &mut saw_title_change,
            &mut saw_cwd_change,
        )
        .await;
    }
    state.osc_events = events_this_iter;

    // ScribeEventListener channel events.
    while let Ok(event) = state.event_rx.try_recv() {
        handle_session_event(event, state, &mut saw_title_change, &mut saw_cwd_change).await;
    }

    // Spec 010 C4: any OSC 52 client→reader command that arrived during
    // PTY processing (after the select! in `next_pty_read_action`
    // returned bytes) gets drained here so the reader stays consistent
    // with `ClipboardBurstState` / `pending_clipboard_*` between chunks.
    while let Ok(cmd) = state.clipboard_command_rx.try_recv() {
        match cmd {
            ClipboardCommand::PromptResponse { request_id, decision } => {
                handle_clipboard_prompt_response(state, request_id, decision).await;
            }
            ClipboardCommand::BridgeReadReply { request_id, payload } => {
                handle_clipboard_bridge_read_reply(state, request_id, payload).await;
            }
            ClipboardCommand::RefreshPolicy { policy } => {
                handle_clipboard_policy_refresh(state, policy);
            }
        }
    }

    // Fallback: title changed but no OSC 7 → read /proc/pid/cwd.
    if saw_title_change && !saw_cwd_change {
        check_proc_cwd(state).await;
    }
}

async fn handle_session_event(
    event: SessionEvent,
    state: &mut PtyReaderState,
    saw_title_change: &mut bool,
    saw_cwd_change: &mut bool,
) {
    match event {
        SessionEvent::Metadata(event) => {
            update_ai_provider_state(state, &event);
            // AiProviderArmed is a server-internal pre-arm signal —
            // no ServerMessage variant exists for it and the client
            // doesn't need to track it.
            if matches!(event, MetadataEvent::AiProviderArmed { .. }) {
                return;
            }
            classify_event(&event, saw_title_change, saw_cwd_change, &mut state.last_proc_cwd);
            send_metadata_event(
                event,
                state.session_id,
                &state.client_writer,
                &state.workspace_manager,
                &state.live_sessions,
            )
            .await;
        }
        SessionEvent::ClipboardStore(kind, text) => {
            handle_clipboard_store(state, kind, text).await;
        }
        SessionEvent::ClipboardLoad(kind, format) => {
            handle_clipboard_load(state, kind, format).await;
        }
        SessionEvent::ColorRequest(index, format) => {
            let color = current_term_color(&state.term, index).await;
            let response = format(color);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
        }
        SessionEvent::PtyWrite(text) => {
            write_term_response(&state.pty_write, state.session_id, text.as_bytes()).await;
        }
        SessionEvent::TextAreaSizeRequest(format) => {
            let size = current_window_size(state).await;
            let response = format(size);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
        }
    }
}

/// Read the cached `clipboard_gating` capability bit for this session's
/// window (spec 010 C7). Returns `false` when no client is attached or the
/// attached client did not advertise support; the caller treats that path
/// as a headless deny per research decision 7.
async fn client_clipboard_gating(state: &PtyReaderState) -> bool {
    state.window_clipboard_gating.read().await.get(&state.window_id).copied().unwrap_or(false)
}

/// Whether *any* client writer is currently attached to this session. Used
/// in tandem with `client_clipboard_gating` to gate OSC 52 prompt and bridge
/// dispatch — both checks must succeed before a `ServerMessage::Clipboard*`
/// variant goes on the wire.
async fn session_has_attached_client(state: &PtyReaderState) -> bool {
    state.client_writer.lock().await.is_some()
}

/// Handle an OSC 52 `ClipboardStore` event (spec 010 contract C4 write arm).
/// Replaces the legacy in-memory `ServerClipboard` store with a policy
/// check, size-cap enforcement, and a dispatch onto either the host
/// clipboard bridge (Allow), the prompt RPC (Prompt — first / cross-op
/// hit), the deferred queue (Prompt — same-op same-burst), the burst-
/// reuse fast path (Prompt — cached decision within `burst_window_ms`),
/// or a silent drop (Deny / headless / oversize / queue-overflow).
//
// @lat: [[server#Sessions#Clipboard Gating]]
async fn handle_clipboard_store(
    state: &mut PtyReaderState,
    kind: alacritty_terminal::term::ClipboardType,
    text: String,
) {
    use scribe_common::config::ClipboardMode;

    let policy = state.clipboard_burst.policy.clone();
    let selection = clipboard_selection_from(kind);

    // FR-009 / FR-015: cap enforcement runs before the policy branch so the
    // size cap covers every write path uniformly — allow-mode forwards,
    // prompt-mode prompts, prompt-mode burst-reuse hits, and the deferred-
    // queue drain on prompt resolution all share this single check (T050).
    // An oversize write is silently dropped (no host clipboard mutation, no
    // PTY reply); the debug log lets operators observe the rejection on
    // demand per UX-002.
    let payload_len = text.len() as u64;
    if payload_len > policy.max_write_bytes {
        debug!(
            session_id = %state.session_id,
            bytes = payload_len,
            cap = policy.max_write_bytes,
            write_mode = ?policy.write_mode,
            selection = ?selection,
            "OSC 52 write rejected: payload exceeds max_write_bytes",
        );
        return;
    }

    let headless =
        !session_has_attached_client(state).await || !client_clipboard_gating(state).await;

    match policy.write_mode {
        ClipboardMode::Deny => {}
        ClipboardMode::Allow => {
            if headless {
                return;
            }
            send_to_client(
                &state.client_writer,
                &ServerMessage::ClipboardBridgeWrite {
                    session_id: state.session_id,
                    selection,
                    payload: text,
                },
            )
            .await;
        }
        ClipboardMode::Prompt => {
            if headless {
                return;
            }
            handle_clipboard_store_prompt(state, selection, text).await;
        }
    }
}

/// Prompt-mode dispatch for an OSC 52 write (spec 010 FR-016 / FR-017):
/// defer onto the bounded queue when a same-op prompt is open, reuse the
/// cached decision when still within the burst window, otherwise open a
/// fresh prompt. Extracted from [`handle_clipboard_store`] to keep both
/// functions inside the line-count / cognitive-complexity budgets.
async fn handle_clipboard_store_prompt(
    state: &mut PtyReaderState,
    selection: scribe_common::protocol::ClipboardSelection,
    text: String,
) {
    use scribe_common::protocol::ClipboardOp;

    if state.clipboard_burst.outstanding_prompt.is_some() {
        defer_clipboard_write_during_prompt(state, selection, text);
        return;
    }
    if let Some(cached) = state.clipboard_burst.reusable_decision(ClipboardOp::Write) {
        apply_clipboard_write_decision(state, cached, selection, text).await;
        return;
    }
    let request_id = allocate_prompt_id();
    let preview = clipboard_write_preview(&text);
    state.clipboard_burst.outstanding_prompt = Some(request_id);
    state.pending_clipboard_prompt = Some(PendingClipboardPrompt {
        request_id,
        op: ClipboardOp::Write,
        selection,
        write_payload: Some(text),
        read_formatter: None,
    });
    send_to_client(
        &state.client_writer,
        &ServerMessage::ClipboardPromptRequest {
            session_id: state.session_id,
            request_id,
            op: ClipboardOp::Write,
            selection,
            preview: Some(preview),
        },
    )
    .await;
}

/// Park an OSC 52 write request behind an open prompt (FR-016). Mismatched
/// ops and queue overflows fall back to the silent-drop path with a
/// `debug!` log so operators can observe the cap being hit.
fn defer_clipboard_write_during_prompt(
    state: &mut PtyReaderState,
    selection: scribe_common::protocol::ClipboardSelection,
    text: String,
) {
    use scribe_common::protocol::ClipboardOp;

    if !matches!(state.pending_clipboard_prompt.as_ref().map(|p| p.op), Some(ClipboardOp::Write)) {
        debug!(
            session_id = %state.session_id,
            "OSC 52 write ignored: prompt in flight for different op",
        );
        return;
    }
    let deferred = crate::clipboard_state::DeferredRequest {
        request_id: allocate_prompt_id(),
        op: ClipboardOp::Write,
        selection,
        payload_for_write: Some(text),
        read_formatter: None,
    };
    if !state.clipboard_burst.try_defer(deferred) {
        debug!(
            session_id = %state.session_id,
            cap = crate::clipboard_state::MAX_PENDING_FOR_PROMPT,
            "OSC 52 write dropped: pending_for_prompt cap reached",
        );
    }
}

/// Handle an OSC 52 `ClipboardLoad` event (spec 010 contract C4 read arm).
//
// @lat: [[server#Sessions#Clipboard Gating]]
async fn handle_clipboard_load(
    state: &mut PtyReaderState,
    kind: alacritty_terminal::term::ClipboardType,
    formatter: ClipboardReplyFormatter,
) {
    use scribe_common::config::ClipboardMode;

    let policy_read = state.clipboard_burst.policy.read_mode;
    let selection = clipboard_selection_from(kind);

    // FR-013 / research decision 7: a denied or headless read short-
    // circuits to an empty OSC 52 reply so the PTY-side program does not
    // see clipboard contents.
    let headless =
        !session_has_attached_client(state).await || !client_clipboard_gating(state).await;

    match policy_read {
        ClipboardMode::Deny => {
            spawn_empty_pty_reply(state, &formatter);
        }
        ClipboardMode::Allow => {
            if headless {
                spawn_empty_pty_reply(state, &formatter);
                return;
            }
            let request_id = allocate_prompt_id();
            state.pending_clipboard_reads.insert(request_id, formatter);
            send_to_client(
                &state.client_writer,
                &ServerMessage::ClipboardBridgeReadRequest {
                    session_id: state.session_id,
                    request_id,
                    selection,
                },
            )
            .await;
        }
        ClipboardMode::Prompt => {
            if headless {
                spawn_empty_pty_reply(state, &formatter);
                return;
            }
            handle_clipboard_load_prompt(state, selection, formatter).await;
        }
    }
}

/// Prompt-mode dispatch for an OSC 52 read (spec 010 FR-016 / FR-017):
/// defer onto the bounded queue when a same-op prompt is open, reuse the
/// cached decision when still within the burst window, otherwise open a
/// fresh prompt. Mirror of [`handle_clipboard_store_prompt`].
async fn handle_clipboard_load_prompt(
    state: &mut PtyReaderState,
    selection: scribe_common::protocol::ClipboardSelection,
    formatter: ClipboardReplyFormatter,
) {
    use scribe_common::protocol::ClipboardOp;

    if state.clipboard_burst.outstanding_prompt.is_some() {
        defer_clipboard_read_during_prompt(state, selection, &formatter);
        return;
    }
    if let Some(cached) = state.clipboard_burst.reusable_decision(ClipboardOp::Read) {
        apply_clipboard_read_decision(state, cached, selection, formatter).await;
        return;
    }
    let request_id = allocate_prompt_id();
    state.clipboard_burst.outstanding_prompt = Some(request_id);
    state.pending_clipboard_prompt = Some(PendingClipboardPrompt {
        request_id,
        op: ClipboardOp::Read,
        selection,
        write_payload: None,
        read_formatter: Some(formatter),
    });
    send_to_client(
        &state.client_writer,
        &ServerMessage::ClipboardPromptRequest {
            session_id: state.session_id,
            request_id,
            op: ClipboardOp::Read,
            selection,
            preview: None,
        },
    )
    .await;
}

/// Park an OSC 52 read request behind an open prompt (FR-016). Mismatched
/// ops empty-reply to unblock the PTY; queue overflows do the same plus a
/// `debug!` log.
fn defer_clipboard_read_during_prompt(
    state: &mut PtyReaderState,
    selection: scribe_common::protocol::ClipboardSelection,
    formatter: &ClipboardReplyFormatter,
) {
    use scribe_common::protocol::ClipboardOp;

    if !matches!(state.pending_clipboard_prompt.as_ref().map(|p| p.op), Some(ClipboardOp::Read)) {
        debug!(
            session_id = %state.session_id,
            "OSC 52 read ignored: prompt in flight for different op",
        );
        spawn_empty_pty_reply(state, formatter);
        return;
    }
    let deferred = crate::clipboard_state::DeferredRequest {
        request_id: allocate_prompt_id(),
        op: ClipboardOp::Read,
        selection,
        payload_for_write: None,
        read_formatter: Some(Arc::clone(formatter)),
    };
    if !state.clipboard_burst.try_defer(deferred) {
        debug!(
            session_id = %state.session_id,
            cap = crate::clipboard_state::MAX_PENDING_FOR_PROMPT,
            "OSC 52 read dropped: pending_for_prompt cap reached",
        );
        spawn_empty_pty_reply(state, formatter);
    }
}

/// Spawn a fire-and-forget task that delivers an empty OSC 52 reply to the
/// PTY for the given formatter. Used by deny / headless / overflow paths
/// where the PTY-side program must not block on a never-emitted reply.
fn spawn_empty_pty_reply(state: &PtyReaderState, formatter: &ClipboardReplyFormatter) {
    let response = formatter("");
    let pty_write = Arc::clone(&state.pty_write);
    let session_id = state.session_id;
    let bytes = response.into_bytes();
    tokio::spawn(async move {
        write_term_response(&pty_write, session_id, &bytes).await;
    });
}

/// Apply a resolved `ClipboardDecision` to a Write request whose payload
/// already passed the size-cap check. Used by the burst-reuse fast path
/// and by `handle_clipboard_prompt_response`'s drain loop.
async fn apply_clipboard_write_decision(
    state: &mut PtyReaderState,
    decision: scribe_common::protocol::ClipboardDecision,
    selection: scribe_common::protocol::ClipboardSelection,
    payload: String,
) {
    use scribe_common::protocol::ClipboardDecision;
    let allowed = matches!(decision, ClipboardDecision::AllowOnce | ClipboardDecision::AlwaysAllow);
    if !allowed {
        // Deny: silent drop (no host clipboard mutation, no PTY reply).
        return;
    }
    send_to_client(
        &state.client_writer,
        &ServerMessage::ClipboardBridgeWrite { session_id: state.session_id, selection, payload },
    )
    .await;
}

/// Apply a resolved `ClipboardDecision` to a Read request. On Allow,
/// forwards a `ClipboardBridgeReadRequest` to the client and stashes the
/// formatter for the reply fan-in; on Deny, formats an empty OSC 52 reply
/// inline.
async fn apply_clipboard_read_decision(
    state: &mut PtyReaderState,
    decision: scribe_common::protocol::ClipboardDecision,
    selection: scribe_common::protocol::ClipboardSelection,
    formatter: ClipboardReplyFormatter,
) {
    use scribe_common::protocol::ClipboardDecision;
    let allowed = matches!(decision, ClipboardDecision::AllowOnce | ClipboardDecision::AlwaysAllow);
    if allowed {
        let bridge_request_id = allocate_prompt_id();
        state.pending_clipboard_reads.insert(bridge_request_id, formatter);
        send_to_client(
            &state.client_writer,
            &ServerMessage::ClipboardBridgeReadRequest {
                session_id: state.session_id,
                request_id: bridge_request_id,
                selection,
            },
        )
        .await;
    } else {
        // Deny: PTY-side program sees an empty OSC 52 reply (UX-002).
        let response = formatter("");
        write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
    }
}

/// Apply the user's `ClipboardPromptResponse` decision (spec 010 C4 prompt
/// resolution). Clears the in-flight prompt slot, records the decision for
/// the FR-017 burst-reuse window, drains every deferred same-op request
/// out of `pending_for_prompt`, and on `AlwaysAllow` / `AlwaysDeny`
/// mutates the in-memory policy snapshot so the next OSC 52 op outside
/// the burst window already sees the new mode without waiting for the
/// `ConfigReloaded` round-trip (T043).
async fn handle_clipboard_prompt_response(
    state: &mut PtyReaderState,
    request_id: scribe_common::protocol::PromptId,
    decision: scribe_common::protocol::ClipboardDecision,
) {
    let Some(pending) = take_matching_pending_prompt(state, request_id) else {
        return;
    };
    state.clipboard_burst.outstanding_prompt = None;
    state.clipboard_burst.record_decision(pending.op, decision);
    apply_always_decision_to_policy(state, pending.op, decision);

    // Apply the decision to the originating prompt first, then drain any
    // requests parked behind it. The same `decision` value flows through
    // both paths so the burst inherits the user's choice (FR-016).
    apply_pending_prompt_decision(state, pending, decision).await;
    drain_pending_for_prompt(state, decision).await;
}

/// Take the in-flight `pending_clipboard_prompt` only if its `request_id`
/// matches; otherwise restore the slot so a correctly-tagged reply can
/// still land. Returns `None` (after logging) when the slot was empty or
/// the id mismatched.
fn take_matching_pending_prompt(
    state: &mut PtyReaderState,
    request_id: scribe_common::protocol::PromptId,
) -> Option<PendingClipboardPrompt> {
    let Some(pending) = state.pending_clipboard_prompt.take() else {
        debug!(
            session_id = %state.session_id,
            ?request_id,
            "ClipboardPromptResponse with no in-flight prompt — ignoring",
        );
        return None;
    };
    if pending.request_id != request_id {
        debug!(
            session_id = %state.session_id,
            ?request_id,
            expected = ?pending.request_id,
            "ClipboardPromptResponse request_id mismatch — ignoring",
        );
        state.pending_clipboard_prompt = Some(pending);
        return None;
    }
    Some(pending)
}

/// Spec 010 T043: flip the in-memory policy axis when the user picks
/// `AlwaysAllow` / `AlwaysDeny`. The client writes the change to disk in
/// parallel; the eventual `ConfigReloaded` round-trip is idempotent
/// against the same value.
fn apply_always_decision_to_policy(
    state: &mut PtyReaderState,
    op: scribe_common::protocol::ClipboardOp,
    decision: scribe_common::protocol::ClipboardDecision,
) {
    use scribe_common::config::ClipboardMode;
    use scribe_common::protocol::{ClipboardDecision, ClipboardOp};

    let new_mode = match decision {
        ClipboardDecision::AlwaysAllow => ClipboardMode::Allow,
        ClipboardDecision::AlwaysDeny => ClipboardMode::Deny,
        ClipboardDecision::AllowOnce | ClipboardDecision::DenyOnce => return,
    };
    match op {
        ClipboardOp::Read => state.clipboard_burst.policy.read_mode = new_mode,
        ClipboardOp::Write => state.clipboard_burst.policy.write_mode = new_mode,
    }
}

/// Apply a resolved decision to the originating pending prompt — write
/// payloads are forwarded via [`apply_clipboard_write_decision`]; reads
/// are forwarded via [`apply_clipboard_read_decision`] when a formatter
/// is present.
async fn apply_pending_prompt_decision(
    state: &mut PtyReaderState,
    pending: PendingClipboardPrompt,
    decision: scribe_common::protocol::ClipboardDecision,
) {
    use scribe_common::protocol::ClipboardOp;

    match pending.op {
        ClipboardOp::Write => {
            if let Some(payload) = pending.write_payload {
                apply_clipboard_write_decision(state, decision, pending.selection, payload).await;
            }
        }
        ClipboardOp::Read => {
            if let Some(formatter) = pending.read_formatter {
                apply_clipboard_read_decision(state, decision, pending.selection, formatter).await;
            } else {
                debug!(
                    session_id = %state.session_id,
                    "ClipboardPromptResponse Read path missing formatter — empty reply skipped",
                );
            }
        }
    }
}

/// Drain every deferred OSC 52 request out of the burst queue and replay
/// the same `decision` against each one (FR-016 / T040). The queue is
/// emptied by [`crate::clipboard_state::ClipboardBurstState::drain_pending`].
async fn drain_pending_for_prompt(
    state: &mut PtyReaderState,
    decision: scribe_common::protocol::ClipboardDecision,
) {
    use scribe_common::protocol::ClipboardOp;

    for deferred in state.clipboard_burst.drain_pending() {
        let request_id = deferred.request_id;
        debug!(
            session_id = %state.session_id,
            ?request_id,
            op = ?deferred.op,
            ?decision,
            "draining deferred OSC 52 request against resolved prompt decision",
        );
        match deferred.op {
            ClipboardOp::Write => {
                if let Some(payload) = deferred.payload_for_write {
                    apply_clipboard_write_decision(state, decision, deferred.selection, payload)
                        .await;
                }
            }
            ClipboardOp::Read => {
                if let Some(formatter) = deferred.read_formatter {
                    apply_clipboard_read_decision(state, decision, deferred.selection, formatter)
                        .await;
                }
            }
        }
    }
}

/// Apply the client's `ClipboardBridgeReadReply` (spec 010 C4 read fan-in).
async fn handle_clipboard_bridge_read_reply(
    state: &mut PtyReaderState,
    request_id: scribe_common::protocol::PromptId,
    payload: Result<String, scribe_common::protocol::BridgeError>,
) {
    let Some(formatter) = state.pending_clipboard_reads.remove(&request_id) else {
        debug!(
            session_id = %state.session_id,
            ?request_id,
            "ClipboardBridgeReadReply with no matching pending read",
        );
        return;
    };
    // `BridgeError` collapses onto an empty payload per UX-002 and research
    // decision 7's headless mapping.
    let text = payload.unwrap_or_default();
    let response = formatter(&text);
    write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
}

fn update_ai_provider_state(state: &mut PtyReaderState, event: &MetadataEvent) {
    // ai_provider lifecycle:
    //
    //   AiStateChanged                          → SET (AI tool announced itself)
    //   AiProviderArmed { provider }            → SET (shell preexec pre-arm,
    //                                                  covers `<tool> --resume`'s
    //                                                  pre-OSC-1337 ED 3)
    //   AiStateCleared                          → CLEAR (explicit inactive)
    //   PromptMark { kind: PromptStart, .. }    → CLEAR (shell prompt returned;
    //                                                    the AI tool has exited
    //                                                    and we're back in plain
    //                                                    shell — vim/less/etc.
    //                                                    must not be filtered)
    //
    // On both clear paths, reset the preserved-scrollback trim baseline so a
    // stale AI-era baseline can't trim a later main-screen redraw back into
    // AI-era content. Without this reset, `preserved_ai_scrollback` would still
    // hold a baseline pointing into history that no longer represents an active
    // AI epoch, and a subsequent same-epoch ED 3 (after pre-arm re-engages)
    // would trim to the wrong row.
    match event {
        MetadataEvent::AiStateChanged(ai_state) => {
            state.ai_provider = Some(ai_state.provider);
        }
        MetadataEvent::AiProviderArmed { provider } => {
            state.ai_provider = Some(*provider);
        }
        MetadataEvent::AiStateCleared
        | MetadataEvent::PromptMark { kind: PromptMarkKind::PromptStart, .. } => {
            state.ai_provider = None;
            state.preserved_ai_scrollback.reset();
            state.pending_ai_scrollback_baseline = false;
        }
        _ => {}
    }
}

fn prepare_preserved_ai_scrollback_epoch(state: &mut PtyReaderState) {
    if state.osc_events.iter().any(metadata_starts_ai_scrollback_epoch) {
        reset_preserved_ai_scrollback_epoch(state);
    }
}

fn reset_preserved_ai_scrollback_epoch(state: &mut PtyReaderState) {
    state.preserved_ai_scrollback.reset();
    state.pending_ai_scrollback_baseline = false;
}

fn metadata_starts_ai_scrollback_epoch(event: &MetadataEvent) -> bool {
    match event {
        MetadataEvent::AiStateChanged(ai_state) => matches!(
            ai_state.state,
            AiState::IdlePrompt
                | AiState::WaitingForInput
                | AiState::PermissionPrompt
                | AiState::Error
        ),
        MetadataEvent::AiStateCleared | MetadataEvent::PromptReceived { .. } => true,
        _ => false,
    }
}

fn should_apply_ed3_filter(ai_provider: Option<AiProvider>, chunk_has_ed3_provider: bool) -> bool {
    ai_provider_uses_ed3_filter(ai_provider) || chunk_has_ed3_provider
}

fn chunk_mentions_ed3_provider(events: &[MetadataEvent]) -> bool {
    events.iter().any(|event| match event {
        MetadataEvent::AiStateChanged(ai_state) => {
            ai_provider_uses_ed3_filter(Some(ai_state.provider))
        }
        MetadataEvent::AiProviderArmed { provider } => ai_provider_uses_ed3_filter(Some(*provider)),
        _ => false,
    })
}

async fn current_term_color(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    index: usize,
) -> alacritty_terminal::vte::ansi::Rgb {
    let term_guard = term.lock().await;
    if index >= alacritty_terminal::term::color::COUNT {
        return alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 };
    }

    if let Some(color) = term_guard.colors()[index] {
        return color;
    }
    drop(term_guard);

    fallback_term_color(index).unwrap_or(alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 })
}

fn fallback_term_color(index: usize) -> Option<alacritty_terminal::vte::ansi::Rgb> {
    let config = scribe_config::load_config().ok()?;
    let theme = scribe_config::resolve_theme(&config);

    theme_color_for_index(&theme, index).map(theme_color_to_rgb)
}

fn theme_color_for_index(theme: &scribe_common::theme::Theme, index: usize) -> Option<[f32; 4]> {
    use alacritty_terminal::vte::ansi::NamedColor;

    match index {
        0..=15 => theme.ansi_colors.get(index).copied(),
        x if x == NamedColor::Foreground as usize || x == NamedColor::BrightForeground as usize => {
            Some(theme.foreground)
        }
        x if x == NamedColor::Background as usize => Some(theme.background),
        x if x == NamedColor::Cursor as usize => Some(theme.cursor),
        x if x == NamedColor::DimForeground as usize => Some(dim_theme_color(theme.foreground)),
        _ => dim_ansi_theme_color(theme, index),
    }
}

fn dim_ansi_theme_color(theme: &scribe_common::theme::Theme, index: usize) -> Option<[f32; 4]> {
    use alacritty_terminal::vte::ansi::NamedColor;

    let base_index = match index {
        x if x == NamedColor::DimBlack as usize => 0,
        x if x == NamedColor::DimRed as usize => 1,
        x if x == NamedColor::DimGreen as usize => 2,
        x if x == NamedColor::DimYellow as usize => 3,
        x if x == NamedColor::DimBlue as usize => 4,
        x if x == NamedColor::DimMagenta as usize => 5,
        x if x == NamedColor::DimCyan as usize => 6,
        x if x == NamedColor::DimWhite as usize => 7,
        _ => return None,
    };

    theme.ansi_colors.get(base_index).copied().map(dim_theme_color)
}

fn dim_theme_color(color: [f32; 4]) -> [f32; 4] {
    [color[0] * 0.67, color[1] * 0.67, color[2] * 0.67, color[3]]
}

fn theme_color_to_rgb(color: [f32; 4]) -> alacritty_terminal::vte::ansi::Rgb {
    alacritty_terminal::vte::ansi::Rgb {
        r: scribe_common::theme::channel_to_u8(color[0]),
        g: scribe_common::theme::channel_to_u8(color[1]),
        b: scribe_common::theme::channel_to_u8(color[2]),
    }
}

async fn current_window_size(state: &PtyReaderState) -> alacritty_terminal::event::WindowSize {
    let term_guard = state.term.lock().await;
    let rows = term_guard.grid().screen_lines();
    let cols = term_guard.grid().columns();
    alacritty_terminal::event::WindowSize {
        num_lines: u16::try_from(rows).unwrap_or(u16::MAX),
        num_cols: u16::try_from(cols).unwrap_or(u16::MAX),
        cell_width: state.cell_width.max(1),
        cell_height: state.cell_height.max(1),
    }
}

async fn write_term_response(
    pty_write: &Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    session_id: SessionId,
    data: &[u8],
) {
    let mut writer = pty_write.lock().await;
    if let Err(e) = writer.write_all(data).await {
        debug!(%session_id, error = %e, "failed to write terminal response to PTY");
    }
}

/// Update the `saw_title_change` / `saw_cwd_change` flags and keep
/// `last_proc_cwd` in sync with any OSC 7 events.
fn classify_event(
    event: &MetadataEvent,
    saw_title: &mut bool,
    saw_cwd: &mut bool,
    last_cwd: &mut Option<std::path::PathBuf>,
) {
    match event {
        MetadataEvent::TitleChanged(_) => *saw_title = true,
        MetadataEvent::CwdChanged(cwd) => {
            *saw_cwd = true;
            *last_cwd = Some(cwd.clone());
        }
        _ => {}
    }
}

/// Read `/proc/{pid}/cwd` and synthesise a `CwdChanged` event when the CWD
/// has changed since the last check.  Called when the shell emits a title
/// change (OSC 0) but no OSC 7, so workspace naming still works for shells
/// that only set the window title in PS1.
#[cfg(target_os = "linux")]
async fn check_proc_cwd(state: &mut PtyReaderState) {
    let proc_cwd = std::path::PathBuf::from(format!("/proc/{}/cwd", state.child_pid));
    let Ok(cwd) = std::fs::read_link(&proc_cwd) else {
        return;
    };
    if state.last_proc_cwd.as_ref() == Some(&cwd) {
        return;
    }
    state.last_proc_cwd = Some(cwd.clone());
    let event = MetadataEvent::CwdChanged(cwd);
    send_metadata_event(
        event,
        state.session_id,
        &state.client_writer,
        &state.workspace_manager,
        &state.live_sessions,
    )
    .await;
}

/// macOS fallback: use `proc_pidinfo` with `PROC_PIDVNODEPATHINFO` to read
/// the child process CWD, then synthesise a `CwdChanged` event when it differs
/// from the last known value.
#[cfg(target_os = "macos")]
async fn check_proc_cwd(state: &mut PtyReaderState) {
    let Some(cwd) = crate::macos_proc::macos_proc_cwd(state.child_pid) else {
        return;
    };
    if state.last_proc_cwd.as_ref() == Some(&cwd) {
        return;
    }
    state.last_proc_cwd = Some(cwd.clone());
    let event = MetadataEvent::CwdChanged(cwd);
    send_metadata_event(
        event,
        state.session_id,
        &state.client_writer,
        &state.workspace_manager,
        &state.live_sessions,
    )
    .await;
}

/// Stub for platforms other than Linux and macOS — no CWD fallback available.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn check_proc_cwd(_state: &mut PtyReaderState) {}

/// Maximum number of parent directories to traverse when searching for a
/// `.git/HEAD` file. Prevents unbounded walks on deep or unusual directory
/// trees where no git repository is ever found.
const GIT_WALK_DEPTH_LIMIT: usize = 50;

/// Detect the current git branch by walking up from `cwd` looking for `.git/HEAD`.
///
/// Returns `Some(branch_name)` if on a named branch, `Some(short_sha)` if in
/// detached HEAD state, or `None` if not inside a git repository.
/// Stops after `GIT_WALK_DEPTH_LIMIT` iterations to avoid walking all the
/// way to `/` on very deep directory trees.
pub fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    let mut depth = 0usize;
    loop {
        if depth >= GIT_WALK_DEPTH_LIMIT {
            return None;
        }
        depth += 1;

        let head = dir.join(".git/HEAD");
        if let Ok(content) = std::fs::read_to_string(&head) {
            return content
                .strip_prefix("ref: refs/heads/")
                .map(|b| b.trim().to_owned())
                .or_else(|| Some(content.trim().chars().take(8).collect()));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Carry forward optional `AiProcessState` metadata (`context`, `model`,
/// `tool`, `agent`, `conversation_id`) from the previously-stored live
/// session state when the incoming `AiStateChanged` left those fields as
/// `None`. State-only hooks emit `<Provider>State=<state>` without any
/// metadata, which would otherwise clobber the live `context=NN` last
/// set by the statusLine producer.
async fn merge_partial_ai_state(
    server_msg: &mut ServerMessage,
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) {
    let ServerMessage::AiStateChanged { ai_state, .. } = server_msg else {
        return;
    };
    let prev = live_sessions.read().await.get(&session_id).and_then(|s| s.ai_state.clone());
    if let Some(prev) = prev {
        ai_state.merge_partial_from_previous(&prev);
    }
}

/// Apply a context-only refresh from a status-line / usage-poll producer.
///
/// The refresh patches `context` on the live `AiProcessState` for the
/// matching provider and re-broadcasts a full `AiStateChanged` so connected
/// clients see the new percentage without the producer ever asserting a
/// state of its own. Three drop conditions, all silent:
///
/// - The session has no live `ai_state` yet — nothing to patch. The first
///   real state event (Stop hook, `PreToolUse`, etc.) will establish state;
///   the next refresh will then take effect.
/// - The live state's provider differs from the refresh's provider. Cross-
///   provider context bleed (e.g. Codex's `CodexContext=NN` arriving while
///   the live state is Claude's) is rejected as a defensive guard.
/// - The session is gone (closed during the await race).
async fn send_ai_context_change(
    provider: AiProvider,
    context: u8,
    session_id: SessionId,
    client_writer: &ClientWriter,
    live_sessions: &LiveSessionRegistry,
) {
    let updated_state = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return;
        };
        let Some(ai_state) = session.ai_state.as_mut() else {
            return;
        };
        if ai_state.provider != provider {
            return;
        }
        ai_state.context = Some(context);
        ai_state.clone()
    };
    let server_msg = ServerMessage::AiStateChanged { session_id, ai_state: updated_state };
    send_to_client(client_writer, &server_msg).await;
}

/// Persist metadata from a `ServerMessage` into the live session registry.
async fn persist_session_metadata(
    server_msg: &ServerMessage,
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) {
    match server_msg {
        ServerMessage::TitleChanged { title, .. } if !title.trim().is_empty() => {
            update_live_session(session_id, live_sessions, |session| {
                title.clone_into(&mut session.title);
            })
            .await;
        }
        ServerMessage::CodexTaskLabelChanged { task_label, .. }
            if !task_label.trim().is_empty() =>
        {
            update_live_session(session_id, live_sessions, |session| {
                session.task_label = Some(task_label.clone());
            })
            .await;
        }
        ServerMessage::TaskLabelChanged { task_label, .. } if !task_label.trim().is_empty() => {
            update_live_session(session_id, live_sessions, |session| {
                session.task_label = Some(task_label.clone());
            })
            .await;
        }
        ServerMessage::CodexTaskLabelCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.task_label = None;
            })
            .await;
        }
        ServerMessage::TaskLabelCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.task_label = None;
            })
            .await;
        }
        ServerMessage::CwdChanged { cwd, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.cwd = Some(cwd.clone());
            })
            .await;
        }
        ServerMessage::SessionContextChanged { context, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.context = Some(context.clone());
            })
            .await;
        }
        ServerMessage::AiStateChanged { ai_state, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.ai_state = Some(ai_state.clone());
            })
            .await;
        }
        ServerMessage::AiStateCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.ai_state = None;
            })
            .await;
        }
        _ => {}
    }
}

async fn update_live_session(
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
    update: impl FnOnce(&mut LiveSession),
) {
    if let Some(session) = live_sessions.write().await.get_mut(&session_id) {
        update(session);
    }
}

/// Convert a `MetadataEvent` to a `ServerMessage` and send it.
/// For `CwdChanged`, also notifies the workspace manager and sends git branch.
/// Workspace naming always runs (even when detached) so names are ready on
/// reconnect. Client messages are only sent when attached.
///
/// Exposed publicly so `hook_ingress` can feed the same pipeline.
pub async fn send_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
    client_writer: &ClientWriter,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
) {
    // Context-only refreshes patch the existing live state instead of
    // creating a new `AiStateChanged`. They never carry a state value, so
    // they cannot synthesize one — when no live state has been established
    // yet, the event is dropped silently.
    if let MetadataEvent::AiContextChanged { provider, context } = event {
        send_ai_context_change(provider, context, session_id, client_writer, live_sessions).await;
        return;
    }

    // PromptStart from shell integration after the AI tool exits means the
    // pane is back at a plain shell prompt. `update_ai_provider_state`
    // already drops the server-side ai_provider on this signal (see the
    // ai_provider lifecycle comment), but without an accompanying
    // ServerMessage the client keeps its prompt bar, notification tracker,
    // and cold-restart launch binding pointing at a dead AI tool. Capture
    // the transition before persistence so the follow-up emission below
    // brings the client view in line with the server's interpretation.
    let synthesize_ai_cleared =
        matches!(&event, MetadataEvent::PromptMark { kind: PromptMarkKind::PromptStart, .. })
            && live_sessions.read().await.get(&session_id).is_some_and(|s| s.ai_state.is_some());

    let Some((mut server_msg, cwd_for_workspace)) = convert_metadata_event(event, session_id)
    else {
        return;
    };

    merge_partial_ai_state(&mut server_msg, session_id, live_sessions).await;

    persist_session_metadata(&server_msg, session_id, live_sessions).await;

    send_to_client(client_writer, &server_msg).await;

    if synthesize_ai_cleared {
        let clear_msg = ServerMessage::AiStateCleared { session_id };
        persist_session_metadata(&clear_msg, session_id, live_sessions).await;
        send_to_client(client_writer, &clear_msg).await;
    }

    if let Some(cwd) = cwd_for_workspace {
        // Send git branch information for the new CWD.
        let branch = detect_git_branch(&cwd);
        let git_msg = ServerMessage::GitBranch { session_id, branch };
        send_to_client(client_writer, &git_msg).await;

        // Always update workspace naming, even when detached.
        let named_msg = {
            let mut wm = workspace_manager.write().await;
            wm.on_cwd_changed(session_id, &cwd)
        };
        if let Some(msg) = named_msg {
            send_to_client(client_writer, &msg).await;
        }
    }
}

/// Convert a `MetadataEvent` to a `ServerMessage` and an optional CWD.
/// Returns `None` for server-internal events that have no client-facing
/// `ServerMessage` (e.g. `AiProviderArmed`). The inner second tuple element
/// is `Some(cwd)` only for `CwdChanged` events, which also need workspace
/// naming and git-branch updates.
fn convert_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
) -> Option<(ServerMessage, Option<std::path::PathBuf>)> {
    match event {
        MetadataEvent::CwdChanged(cwd) => {
            let msg = ServerMessage::CwdChanged { session_id, cwd: cwd.clone() };
            Some((msg, Some(cwd)))
        }
        MetadataEvent::SessionContextChanged(context) => {
            Some((ServerMessage::SessionContextChanged { session_id, context }, None))
        }
        MetadataEvent::TitleChanged(title) => {
            Some((ServerMessage::TitleChanged { session_id, title }, None))
        }
        MetadataEvent::TaskLabelChanged { provider: AiProvider::CodexCode, label }
        | MetadataEvent::CodexTaskLabelChanged(label) => {
            Some((ServerMessage::CodexTaskLabelChanged { session_id, task_label: label }, None))
        }
        MetadataEvent::TaskLabelChanged { provider, label } => Some((
            ServerMessage::TaskLabelChanged { session_id, provider, task_label: label },
            None,
        )),
        MetadataEvent::TaskLabelCleared { provider: AiProvider::CodexCode }
        | MetadataEvent::CodexTaskLabelCleared => {
            Some((ServerMessage::CodexTaskLabelCleared { session_id }, None))
        }
        MetadataEvent::TaskLabelCleared { provider } => {
            Some((ServerMessage::TaskLabelCleared { session_id, provider }, None))
        }
        MetadataEvent::AiStateChanged(ai_state) => {
            Some((ServerMessage::AiStateChanged { session_id, ai_state }, None))
        }
        MetadataEvent::AiStateCleared => Some((ServerMessage::AiStateCleared { session_id }, None)),
        // Neither variant has a direct client-facing `ServerMessage`.
        // `AiProviderArmed` is a server-internal pre-arm signal handled inside
        // `update_ai_provider_state`; `handle_session_event` short-circuits
        // before reaching here, so this arm is purely defensive.
        // `AiContextChanged` is patched onto the live `AiProcessState` and
        // re-broadcast as `AiStateChanged` directly from `send_metadata_event`.
        MetadataEvent::AiProviderArmed { .. } | MetadataEvent::AiContextChanged { .. } => None,
        MetadataEvent::Bell => Some((ServerMessage::Bell { session_id }, None)),
        MetadataEvent::PromptMark { kind, click_events, exit_code } => {
            Some((ServerMessage::PromptMark { session_id, kind, click_events, exit_code }, None))
        }
        MetadataEvent::PromptReceived { provider, text } => {
            Some((ServerMessage::PromptReceived { session_id, provider, text }, None))
        }
    }
}

// ── Handoff helpers ──────────────────────────────────────────────

/// Create a new, empty live session registry.
pub fn new_live_session_registry() -> LiveSessionRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Create a new empty `ConnectedClients` registry.
pub fn new_connected_clients() -> ConnectedClients {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Create a new empty `WindowClipboardGating` registry. Spec 010 C7.
pub fn new_window_clipboard_gating() -> WindowClipboardGating {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Create a new empty `WindowControllers` registry. Feature 013.
pub fn new_window_controllers() -> WindowControllers {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Serialise all live sessions for a hot-reload handoff.
///
/// Returns `(sessions, fds)` where the fds are in the same order as the
/// session vec. The caller must send these fds via `SCM_RIGHTS`.
pub async fn serialize_live_for_handoff(
    live_sessions: &LiveSessionRegistry,
) -> (Vec<HandoffSession>, Vec<Arc<OwnedFd>>) {
    let sessions = live_sessions.read().await;
    let mut handoff_sessions = Vec::with_capacity(sessions.len());
    let mut fds = Vec::with_capacity(sessions.len());

    for (&session_id, live) in sessions.iter() {
        let term = live.term.lock().await;
        let snapshot = snapshot_term(&term);
        let cols = u16::try_from(term.grid().columns()).unwrap_or(u16::MAX);
        let rows = u16::try_from(term.grid().screen_lines()).unwrap_or(u16::MAX);
        drop(term);

        // Encode as a v5 replay (compressed ANSI). If encoding fails, log and
        // leave session_replay None — the receiver will fall back to the
        // legacy snapshot field and still produce a working session.
        let session_replay = match scribe_common::screen_replay::build_session_replay(&snapshot) {
            Ok(replay) => Some(replay),
            Err(e) => {
                tracing::warn!(%session_id, "build_session_replay failed: {e}");
                None
            }
        };

        let has_ai_state = live.ai_state.is_some();
        tracing::debug!(%session_id, has_ai_state, "serializing live session for handoff");

        handoff_sessions.push(HandoffSession {
            session_id,
            workspace_id: live.workspace_id,
            child_pid: live.child_pid,
            cols,
            rows,
            cell_width: live.cell_width,
            cell_height: live.cell_height,
            snapshot: None,
            session_replay,
            title: Some(live.title.clone()),
            shell_name: live.shell_name.clone(),
            task_label: live.task_label.clone(),
            codex_task_label: live.task_label.clone(),
            cwd: live.cwd.clone(),
            context: live.context.clone(),
            ai_state: live.ai_state.clone(),
            ai_provider_hint: live
                .ai_state
                .as_ref()
                .map(|state| state.provider)
                .or(live.ai_provider_hint),
        });

        fds.push(Arc::clone(&live.resize_fd));
    }

    (handoff_sessions, fds)
}

/// Defuse all Pty objects so the old server's exit does not send `SIGHUP` to
/// child processes. Call after a successful handoff, before shutdown.
///
/// `alacritty_terminal::tty::Pty::drop()` explicitly calls
/// `kill(child_pid, SIGHUP)`. Since the new server already holds the PTY
/// master fds (via `SCM_RIGHTS`), the children must stay alive.
/// `std::mem::forget` prevents the `Drop` impl from running.
/// Move all sessions from the `SessionManager` into the live registry and
/// start their PTY reader tasks in detached mode (no client writer).
///
/// Called at the start of `run_server_loop` so that sessions restored from a
/// hot-reload handoff are available for `ListSessions` / `AttachSessions`
/// before any client connects. For a normal (non-upgrade) startup this is a
/// no-op because the `SessionManager` starts empty.
pub async fn activate_pending_sessions(
    session_manager: &SessionManager,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    live_sessions: &LiveSessionRegistry,
    window_clipboard_gating: &WindowClipboardGating,
) {
    let pending = session_manager.pending_session_ids().await;

    for (session_id, workspace_id) in pending {
        if let Some(session) = session_manager.take_session(session_id).await {
            // Look up the handoff-restored window owner. Falls back to a
            // fresh window id if the workspace manager somehow lost the
            // mapping — handoff-restored sessions carry
            // `env_envelope_id = None`, so close-time envelope delete is a
            // no-op in that case anyway.
            let window_id = workspace_manager
                .read()
                .await
                .window_for_session(session_id)
                .unwrap_or_else(WindowId::new);
            start_session(
                StartSessionIds { session: session_id, workspace: workspace_id, window: window_id },
                session,
                InitialAttachment { writer: None, attached_ids: None },
                SessionRuntimeContext { workspace_manager, live_sessions, window_clipboard_gating },
            )
            .await;
            info!(%session_id, "activated restored session (detached)");
        }
    }
}

pub async fn defuse_for_handoff(live_sessions: &LiveSessionRegistry) {
    let mut sessions = live_sessions.write().await;
    for (&session_id, session) in sessions.iter_mut() {
        if let Some(pty) = session.pty.take() {
            // Wrap in ManuallyDrop to prevent Pty::drop() from running.
            // ManuallyDrop does not call the inner type's Drop on scope exit.
            let _defused = std::mem::ManuallyDrop::new(pty);
            info!(%session_id, "defused Pty to prevent SIGHUP on exit");
        }
    }
}

/// Spawn the long-running env-status broadcaster (T036).
///
/// Subscribes to the [`crate::env_store::EnvStoreState`]'s status-transition
/// broadcast channel and, for each `(session_id, internal_state)` tick,
/// looks up the owning session's [`ClientWriter`] from `live_sessions` and
/// sends `ServerMessage::EnvStatus` to the client. Mirrors the fail-open
/// pattern used by [`crate::hook_ingress::handle`]: a missing live-session
/// entry (session closed between transition and forward) is logged at
/// debug and the event is dropped.
///
/// The task ends only when the broadcast sender is dropped (i.e. the
/// `EnvStoreState` itself goes away on server shutdown). `Lagged` errors
/// are also logged at debug and recovery continues — the current status is
/// always retrievable via [`crate::env_store::EnvStoreState::get_status`],
/// so a missed broadcast is informational only.
pub fn spawn_env_status_forwarder(
    env_store: &Arc<crate::env_store::EnvStoreState>,
    live_sessions: LiveSessionRegistry,
) {
    let mut rx = env_store.subscribe_status();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((session_id, internal_state)) => {
                    forward_env_status(&live_sessions, session_id, internal_state).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // Subscriber fell behind by more than the channel
                    // capacity. The current status is recoverable via
                    // `EnvStoreState::get_status`; we just log and resume.
                    debug!(
                        target: "scribe_server::ipc_server",
                        skipped,
                        "env-status forwarder lagged; resuming"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // All senders dropped — the env-store is gone. Server
                    // is shutting down; exit cleanly.
                    debug!(
                        target: "scribe_server::ipc_server",
                        "env-status broadcast closed; forwarder exiting"
                    );
                    return;
                }
            }
        }
    });
}

/// Helper for the env-status forwarder loop: resolves the session's client
/// writer and forwards one transition. Extracted from
/// [`spawn_env_status_forwarder`] to flatten the loop body and keep block
/// nesting under Clippy's `excessive_nesting` threshold.
async fn forward_env_status(
    live_sessions: &LiveSessionRegistry,
    session_id: SessionId,
    internal_state: crate::env_store::EnvStatusState,
) {
    let client_writer = {
        let sessions = live_sessions.read().await;
        sessions.get(&session_id).map(|s| Arc::clone(&s.client_writer))
    };
    let Some(client_writer) = client_writer else {
        // Session closed between the persist task's transition emit and
        // our forward — drop silently. Mirrors `hook_ingress::handle`'s
        // contract.
        debug!(
            target: "scribe_server::ipc_server",
            ?session_id,
            "EnvStatus transition for unknown session — dropped"
        );
        return;
    };

    let msg = ServerMessage::EnvStatus { session_id, state: env_status_to_wire(&internal_state) };
    send_to_client(&client_writer, &msg).await;
    debug!(
        target: "scribe_server::ipc_server",
        ?session_id,
        ?internal_state,
        "forwarded EnvStatus transition to client"
    );
}

/// Convert the server-internal [`crate::env_store::EnvStatusState`] to its
/// wire-protocol counterpart for emission on
/// [`ServerMessage::EnvStatus`]. Kept as a free function so the `env_store`
/// module stays free of `scribe_common::protocol` imports.
fn env_status_to_wire(
    s: &crate::env_store::EnvStatusState,
) -> scribe_common::protocol::EnvStatusState {
    use scribe_common::protocol::EnvStatusState as Wire;
    match s {
        crate::env_store::EnvStatusState::Active => Wire::Active,
        crate::env_store::EnvStatusState::Degraded { reason } => {
            Wire::Degraded { reason: reason.clone() }
        }
    }
}

/// Decide which `WindowId` to assign to a connecting client, and which
/// other unconnected windows should be spawned as separate processes.
///
/// When `hello_window_id` is `Some`, the client already knows its ID
/// (e.g. it was launched with `--window-id`) and may claim it only if no
/// current client owns that window. When `None`, this is a fresh launch — if
/// there are unconnected windows with sessions (restart scenario), the client
/// adopts one instead of creating a new ID.
fn resolve_window_assignment<V>(
    hello_window_id: Option<WindowId>,
    windows_with_sessions: &HashSet<WindowId>,
    connected: &HashMap<WindowId, V>,
) -> (WindowId, Vec<WindowId>) {
    let next_unconnected = || {
        windows_with_sessions
            .iter()
            .find(|wid| !connected.contains_key(wid))
            .copied()
            .unwrap_or_else(WindowId::new)
    };
    let assigned = match hello_window_id {
        Some(window_id) if !connected.contains_key(&window_id) => window_id,
        Some(window_id) => {
            warn!(%window_id, "requested window is already connected; assigning a different window");
            next_unconnected()
        }
        None => next_unconnected(),
    };

    let other_windows: Vec<WindowId> = windows_with_sessions
        .iter()
        .filter(|wid| **wid != assigned && !connected.contains_key(wid))
        .copied()
        .collect();

    (assigned, other_windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::framing::read_message;
    use std::os::unix::net::UnixStream as StdUnixStream;

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().unwrap();
        left.set_nonblocking(true).unwrap();
        right.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(left).unwrap(),
            tokio::net::UnixStream::from_std(right).unwrap(),
        )
    }

    #[tokio::test]
    async fn attach_sessions_returns_empty_when_registry_has_no_matching_sessions() {
        let live_sessions = new_live_session_registry();

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::Local(Box::new(write))));
        let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

        let attached = crate::attach_flow::attach_sessions(
            &[SessionId::new()],
            &[],
            &live_sessions,
            crate::attach_flow::AttachClientContext {
                writer: &writer,
                attached_ids: &attached_ids,
            },
        )
        .await;

        assert!(attached.is_empty());
    }

    /// Fresh first launch — no prior sessions exist.
    /// Should create a new window ID and no other windows.
    #[test]
    fn fresh_launch_no_sessions_creates_new_window() {
        let sessions: HashSet<WindowId> = HashSet::new();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        // Should get a new (unique) window ID, and no windows to spawn.
        assert!(!sessions.contains(&assigned), "should be a brand-new ID");
        assert!(others.is_empty());
    }

    /// Restart with 1 window — one unconnected window has sessions.
    /// The connecting client should adopt that window, not create a new one.
    #[test]
    fn restart_single_window_reuses_existing() {
        let w1 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_eq!(assigned, w1, "should reuse the existing window");
        assert!(others.is_empty(), "no other windows to spawn");
    }

    /// Restart with multiple windows — client adopts one, rest in `other_windows`.
    #[test]
    fn restart_multi_window_adopts_one_spawns_rest() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let w3 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2, w3].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert!(sessions.contains(&assigned), "should adopt an existing window");
        assert_eq!(others.len(), 2, "should spawn the other 2 windows");
        assert!(!others.contains(&assigned), "assigned must not appear in others");
        for o in &others {
            assert!(sessions.contains(o), "other_windows must be known windows");
        }
    }

    /// Explicit --window-id always used, even if it doesn't match any session.
    #[test]
    fn explicit_window_id_used_as_is() {
        let w1 = WindowId::new();
        let w_explicit = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(Some(w_explicit), &sessions, &connected);

        assert_eq!(assigned, w_explicit, "should use the explicit ID");
        assert_eq!(others, vec![w1], "unconnected session window should be in others");
    }

    /// New window spawned while another is already connected — should not
    /// steal the connected window's ID.
    #[test]
    fn does_not_steal_connected_window() {
        let w1 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1].into_iter().collect();
        let connected: HashMap<WindowId, bool> = [(w1, true)].into_iter().collect();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_ne!(assigned, w1, "must not steal connected window");
        assert!(others.is_empty(), "w1 is connected so not in others");
    }

    /// Mix of connected and unconnected windows — only adopts an unconnected one.
    #[test]
    fn adopts_unconnected_skips_connected() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2].into_iter().collect();
        let connected: HashMap<WindowId, bool> = [(w1, true)].into_iter().collect();

        let (assigned, others) = resolve_window_assignment(None, &sessions, &connected);

        assert_eq!(assigned, w2, "should adopt the unconnected window");
        assert!(others.is_empty(), "w1 is connected, w2 is assigned — nothing left");
    }

    /// Explicit window-id that matches a session — no duplication in others.
    #[test]
    fn explicit_id_matching_session_not_in_others() {
        let w1 = WindowId::new();
        let w2 = WindowId::new();
        let sessions: HashSet<WindowId> = [w1, w2].into_iter().collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();

        let (assigned, others) = resolve_window_assignment(Some(w1), &sessions, &connected);

        assert_eq!(assigned, w1);
        assert_eq!(others, vec![w2], "only the other unconnected window");
    }

    fn test_writer() -> SharedWriter {
        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        Arc::new(Mutex::new(ClientSink::Local(Box::new(write))))
    }

    /// One claim with its own writer — extracted so the concurrency test
    /// stays flat (no nested async block inside a loop).
    async fn claim_for(
        registry: ConnectedClients,
        all: HashSet<WindowId>,
        requested: WindowId,
    ) -> WindowId {
        let writer = test_writer();
        let (assigned, _) = claim_window(&registry, Some(requested), &all, &writer).await;
        assigned
    }

    /// Defect 1: a window already claimed by one connection must never be
    /// handed to a second connection, even when the claim+register is the
    /// only thing serialising them. Sequential form is deterministic.
    #[tokio::test]
    async fn claim_window_rejects_already_claimed_window() {
        let w1 = WindowId::new();
        let all: HashSet<WindowId> = [w1].into_iter().collect();
        let registry = new_connected_clients();

        let writer_a = test_writer();
        let (assigned_a, _) = claim_window(&registry, Some(w1), &all, &writer_a).await;
        assert_eq!(assigned_a, w1, "first client adopts the requested window");

        let writer_b = test_writer();
        let (assigned_b, _) = claim_window(&registry, Some(w1), &all, &writer_b).await;
        assert_ne!(assigned_b, w1, "second client must NOT get the same window");

        let connected = registry.read().await;
        assert!(
            Arc::ptr_eq(connected.get(&w1).expect("w1 still owned"), &writer_a),
            "w1 must still belong to the original writer, not be overwritten",
        );
    }

    /// Defect 1 under true concurrency: N simultaneous Hellos for the same
    /// window ID must each end up owning a distinct window, and exactly one
    /// must win the requested ID (the rest get fresh IDs).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_for_same_window_never_collide() {
        let w1 = WindowId::new();
        let all: HashSet<WindowId> = [w1].into_iter().collect();
        let registry = new_connected_clients();

        let handles: Vec<_> = (0..16)
            .map(|_| tokio::spawn(claim_for(Arc::clone(&registry), all.clone(), w1)))
            .collect();

        let mut assigned: Vec<WindowId> = Vec::new();
        for h in handles {
            assigned.push(h.await.unwrap());
        }

        let unique: HashSet<WindowId> = assigned.iter().copied().collect();
        assert_eq!(
            unique.len(),
            assigned.len(),
            "every concurrent claim must get a unique window ID; collision = duplicate window",
        );
        assert_eq!(
            assigned.iter().filter(|&&id| id == w1).count(),
            1,
            "exactly one concurrent claim wins the requested window ID",
        );
    }

    /// Defect 3: once a window is legitimately re-adopted by a new client,
    /// a late/duplicate detach carrying the *old* connection's writer must
    /// not evict the new owner. Driven through the real `claim_window` /
    /// `release_window_if_owned` path rather than a hand-built map state.
    #[tokio::test]
    async fn stale_detach_does_not_evict_new_owner() {
        let w1 = WindowId::new();
        let all: HashSet<WindowId> = [w1].into_iter().collect();
        let registry = new_connected_clients();

        // Client A connects and adopts w1.
        let writer_a = test_writer();
        let (a_assigned, _) = claim_window(&registry, Some(w1), &all, &writer_a).await;
        assert_eq!(a_assigned, w1);

        // A disconnects cleanly — it owns w1, so the entry is released.
        {
            let mut connected = registry.write().await;
            assert!(release_window_if_owned(&mut connected, w1, &writer_a));
            assert!(!connected.contains_key(&w1), "owner detach frees the window");
        }

        // Client B reconnects and legitimately re-adopts the now-free w1.
        let writer_b = test_writer();
        let (b_assigned, _) = claim_window(&registry, Some(w1), &all, &writer_b).await;
        assert_eq!(b_assigned, w1, "B adopts w1 once it is free");

        // A late/duplicate detach from A's old writer must NOT evict B.
        {
            let mut connected = registry.write().await;
            let now_empty = release_window_if_owned(&mut connected, w1, &writer_a);
            assert!(!now_empty, "registry not empty — B still owns w1");
            assert!(
                Arc::ptr_eq(connected.get(&w1).expect("w1 retained"), &writer_b),
                "stale detach from the old writer must not evict the new owner",
            );
        }

        // B's own detach releases it.
        {
            let mut connected = registry.write().await;
            assert!(release_window_if_owned(&mut connected, w1, &writer_b));
            assert!(!connected.contains_key(&w1), "owner detach removes the entry");
        }
    }

    #[test]
    fn ai_clear_rewrite_applies_to_claude_and_codex() {
        let claude = AiProcessState::new_with_provider(AiProvider::ClaudeCode, AiState::Processing);
        let codex = AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::Processing);
        let supported_events = [
            MetadataEvent::AiStateChanged(codex.clone()),
            MetadataEvent::AiStateChanged(claude.clone()),
        ];
        let unsupported_events = [MetadataEvent::AiStateCleared];
        let chunk_has_supported = chunk_mentions_ed3_provider(&supported_events);
        let chunk_has_no_supported = chunk_mentions_ed3_provider(&unsupported_events);

        assert!(ai_state_uses_ed3_filter(Some(&claude)));
        assert!(ai_state_uses_ed3_filter(Some(&codex)));
        assert!(!ai_state_uses_ed3_filter(None));
        assert!(chunk_has_supported);
        assert!(!chunk_has_no_supported);
        assert!(should_apply_ed3_filter(None, chunk_has_supported));
        assert!(!should_apply_ed3_filter(None, chunk_has_no_supported));
    }

    #[test]
    fn codex_task_label_metadata_preserves_legacy_wire_variant() {
        let session_id = SessionId::new();
        let Some((changed_message, changed_cwd)) = convert_metadata_event(
            MetadataEvent::TaskLabelChanged {
                provider: AiProvider::CodexCode,
                label: String::from("refactor parser"),
            },
            session_id,
        ) else {
            panic!("convert_metadata_event returned None for TaskLabelChanged");
        };

        assert!(changed_cwd.is_none());
        assert!(matches!(
            changed_message,
            ServerMessage::CodexTaskLabelChanged { session_id: sid, task_label }
                if sid == session_id && task_label == "refactor parser"
        ));

        let Some((cleared_message, cleared_cwd)) = convert_metadata_event(
            MetadataEvent::TaskLabelCleared { provider: AiProvider::CodexCode },
            session_id,
        ) else {
            panic!("convert_metadata_event returned None for TaskLabelCleared");
        };

        assert!(cleared_cwd.is_none());
        assert!(matches!(
            cleared_message,
            ServerMessage::CodexTaskLabelCleared { session_id: sid } if sid == session_id
        ));
    }

    #[tokio::test]
    async fn dispatch_action_routes_to_target_and_acknowledges_requester() {
        let connected = new_connected_clients();
        let window_id = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter =
            Arc::new(Mutex::new(ClientSink::Local(Box::new(request_write))));

        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter =
            Arc::new(Mutex::new(ClientSink::Local(Box::new(target_write))));

        connected.write().await.insert(window_id, Arc::clone(&target_writer));

        handle_dispatch_action(
            Some(window_id),
            AutomationAction::OpenSettings,
            &connected,
            window_id,
            &request_writer,
        )
        .await;

        let routed: ServerMessage = read_message(&mut target_client).await.unwrap();
        assert!(matches!(
            routed,
            ServerMessage::RunAction { action: AutomationAction::OpenSettings }
        ));

        let ack: ServerMessage = read_message(&mut request_client).await.unwrap();
        assert!(
            matches!(ack, ServerMessage::ActionDispatched { window_id: ack_id } if ack_id == window_id)
        );
    }

    #[tokio::test]
    async fn dispatch_action_reports_missing_window() {
        let connected = new_connected_clients();
        let missing_window = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter =
            Arc::new(Mutex::new(ClientSink::Local(Box::new(request_write))));

        handle_dispatch_action(
            Some(missing_window),
            AutomationAction::OpenSettings,
            &connected,
            missing_window,
            &request_writer,
        )
        .await;

        let response: ServerMessage = read_message(&mut request_client).await.unwrap();
        assert!(
            matches!(response, ServerMessage::Error { message } if message.contains("window not connected"))
        );
    }

    /// Codex (and other Ratatui apps) gate their shaded prompt-panel painting
    /// on the OSC 10 (foreground) and OSC 11 (background) query responses.
    /// If `\e]10;?\e\\` doesn't get answered, Codex never sends OSC 11 and never
    /// emits the `\e[48;2;…m` SGR for the panel. These tests verify that the
    /// alacritty → `ScribeEventListener` path emits a well-formed `ColorRequest`
    /// with a formatter that produces the correct wire response.
    fn make_term_with_listener() -> (
        alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>,
        tokio::sync::mpsc::UnboundedReceiver<scribe_pty::event_listener::SessionEvent>,
    ) {
        use alacritty_terminal::Term;
        use alacritty_terminal::grid::Dimensions;
        use scribe_pty::event_listener::{ScribeEventListener, SessionEvent};
        use tokio::sync::mpsc;

        struct Tiny;
        impl Dimensions for Tiny {
            fn total_lines(&self) -> usize {
                1
            }
            fn screen_lines(&self) -> usize {
                1
            }
            fn columns(&self) -> usize {
                1
            }
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let listener = ScribeEventListener::new(SessionId::new(), event_tx);
        let term = Term::new(crate::session_manager::build_term_config(1), &Tiny, listener);
        (term, event_rx)
    }

    #[test]
    fn osc_10_query_emits_color_request_for_foreground_with_well_formed_response() {
        use alacritty_terminal::vte::ansi::{NamedColor, Processor, Rgb};
        use scribe_pty::event_listener::SessionEvent;

        let (mut term, mut event_rx) = make_term_with_listener();
        let mut processor: Processor<alacritty_terminal::vte::ansi::StdSyncHandler> =
            Processor::new();
        processor.advance(&mut term, b"\x1b]10;?\x1b\\");

        let event = event_rx.try_recv().expect("OSC 10 query should emit a ColorRequest event");
        let SessionEvent::ColorRequest(index, formatter) = event else {
            panic!("expected ColorRequest variant");
        };
        assert_eq!(
            index,
            NamedColor::Foreground as usize,
            "OSC 10 must map to NamedColor::Foreground (256)"
        );

        let response = formatter(Rgb { r: 0xeb, g: 0xdb, b: 0xb2 });
        assert!(response.starts_with("\x1b]10;rgb:"), "wire prefix wrong: {response:?}");
        assert!(response.ends_with("\x1b\\"), "ST terminator missing: {response:?}");
        assert!(
            response.contains("ebeb/dbdb/b2b2"),
            "8-bit channels must be encoded as duplicated 16-bit hex pairs: {response:?}"
        );
    }

    #[test]
    fn osc_11_query_emits_color_request_for_background_with_well_formed_response() {
        use alacritty_terminal::vte::ansi::{NamedColor, Processor, Rgb};
        use scribe_pty::event_listener::SessionEvent;

        let (mut term, mut event_rx) = make_term_with_listener();
        let mut processor: Processor<alacritty_terminal::vte::ansi::StdSyncHandler> =
            Processor::new();
        processor.advance(&mut term, b"\x1b]11;?\x1b\\");

        let event = event_rx.try_recv().expect("OSC 11 query should emit a ColorRequest event");
        let SessionEvent::ColorRequest(index, formatter) = event else {
            panic!("expected ColorRequest variant");
        };
        assert_eq!(
            index,
            NamedColor::Background as usize,
            "OSC 11 must map to NamedColor::Background (257)"
        );

        let response = formatter(Rgb { r: 0x28, g: 0x28, b: 0x28 });
        assert!(response.starts_with("\x1b]11;rgb:"), "wire prefix wrong: {response:?}");
        assert!(response.ends_with("\x1b\\"), "ST terminator missing: {response:?}");
        assert!(
            response.contains("2828/2828/2828"),
            "8-bit channels must be encoded as duplicated 16-bit hex pairs: {response:?}"
        );
    }

    /// Verifies the fallback color-lookup table that `current_term_color` uses
    /// when alacritty's runtime palette has no override for the queried index.
    /// If this returned `None` for Foreground / Background, the OSC reply would
    /// degrade to opaque black (see `current_term_color`), which is the most
    /// likely cause of Codex skipping its bg-shading code path.
    #[test]
    fn theme_color_for_index_returns_named_slots_for_osc_10_11_12() {
        use alacritty_terminal::vte::ansi::NamedColor;
        use scribe_common::theme::{Theme, ThemeColors};
        use std::borrow::Cow;

        let fg = [0.92, 0.86, 0.70, 1.0];
        let bg = [0.16, 0.16, 0.16, 1.0];
        let cursor = [0.5, 0.5, 0.5, 1.0];
        let theme = Theme::from_colors(&ThemeColors {
            name: Cow::Borrowed("test"),
            foreground: fg,
            background: bg,
            cursor,
            cursor_accent: bg,
            selection: [0.25, 0.25, 0.28, 1.0],
            selection_foreground: fg,
            ansi_colors: [[0.0, 0.0, 0.0, 1.0]; 16],
        });

        assert_eq!(
            theme_color_for_index(&theme, NamedColor::Foreground as usize),
            Some(fg),
            "OSC 10 must resolve to theme.foreground, not the black fallback"
        );
        assert_eq!(
            theme_color_for_index(&theme, NamedColor::Background as usize),
            Some(bg),
            "OSC 11 must resolve to theme.background, not the black fallback"
        );
        assert_eq!(
            theme_color_for_index(&theme, NamedColor::Cursor as usize),
            Some(cursor),
            "OSC 12 must resolve to theme.cursor, not the black fallback"
        );
    }
}
