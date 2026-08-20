use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
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
    AiLaunchSpec, AutomationAction, BEADS_BOARD_PROTOCOL_VERSION, BeadsBoardState,
    BeadsEpicGraphOutcome, BeadsIssueWrite, BeadsIssueWriteGuards, BeadsIssueWriteResult,
    CiRunDelta, ClientMessage, ControllerInfo, LanPeerInfo, LanRefusal, ParticipantInfo,
    PromptMarkKind, REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, SearchMatch,
    ServerMessage, SessionInfo, ShellTool, TerminalSize, TrustedDeviceInfo, TrustedNetworkInfo,
    WindowInfo, WorkspaceListEntry, WorkspaceTreeNode,
};
use scribe_common::screen::{ScreenCell, ScreenSnapshot};
use scribe_common::socket::current_uid;
use scribe_common::terminal_images::{
    TerminalImageCapabilities, TerminalImagePlacementKind, TerminalImageReplayMessage,
    TerminalScreenKind,
};
use scribe_pty::event_listener::SessionEvent;
use scribe_pty::metadata::MetadataEvent;
use scribe_pty::osc_interceptor::OscInterceptor;

use crate::beads_board::{BeadsBoardCache, DetailLoadResult, load_issue_detail};
use crate::child_identity::{IdentityCheck, check_child_identity};
use crate::child_watch::{ChildExit, ChildExitWatcher};
use crate::git_ref_watcher::GitRefWatcherControl;
use crate::github_ci::{DetailInterest, GithubCiTrackerHandle};
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
use crate::pty_guard::PtyGuard;
use crate::releases::{ReleaseCatalog, ReleaseFetcher};
use crate::search_cache::{SearchSnapshotCache, SessionSearchCache, SnapshotKey};
use crate::session_exit::{CancelWaiter, READER_JOIN_TIMEOUT, ReaderJoin, SessionExitGate};
use crate::session_manager::{
    ManagedSession, SessionLaunchRequest, SessionManager, SessionSlot, build_term_config,
    snapshot_term,
};
use crate::terminal_image_replay;
use crate::terminal_image_sharing::{
    SessionImageSharing, augment_device_attributes, effective_connection_subset,
    images_master_enabled, plan_pty_replies, set_images_master_enabled,
};
use crate::terminal_image_state::{
    ObservedTerminalGridSpan, ProductionTerminalFeed, PtyTerminalImageState, SessionTerminalCommit,
    SessionTerminalError, SessionTerminalOutput, TerminalGridObservation,
    TerminalGridObserverHandle, TerminalImageBoundary, TerminalImageProcessPolicy,
    feed_terminal_image_result_production, flush_terminal_observed_production,
    observe_terminal_resize, process_pty_reader_ingress,
};
use crate::updater::UpdaterHandle;
use crate::workspace_manager::WorkspaceManager;

/// Buffer size for PTY reads. 64 KiB balances throughput and latency.
const PTY_READ_BUF_SIZE: usize = 64 * 1024;

/// Maximum payload size for a single `KeyInput` message. Legitimate keyboard
/// input is never more than a few dozen bytes; pastes are chunked by the client
/// to fit this limit. Capping at 4 KiB prevents a rogue client from writing
/// 16 MiB (the frame limit) to the PTY in one shot.
const MAX_KEY_INPUT_BYTES: usize = 4 * 1024;

/// Maximum simultaneous LONG-LIVED local IPC connections — the ones that claim a
/// window (`Hello`, or a legacy no-`Hello` first frame). Prevents a same-UID
/// attacker from exhausting memory/tasks by opening thousands of connections;
/// these are also the only local connections that can grow a large output queue,
/// so this cap still bounds the server's per-connection memory. Transient
/// no-window connections are charged to [`MAX_TRANSIENT_CONNECTIONS`] instead
/// (spec 017 US5-5).
const MAX_CONNECTIONS: usize = 32;

/// Maximum simultaneous TRANSIENT local connections — the one-shot no-`Hello`
/// actions that answer at most one frame and never register a window, dominated
/// by `scribe-hook-helper`'s `HookEvent` sends (one connection per hook firing,
/// several per AI turn across every live shell). A SEPARATE semaphore from
/// [`MAX_CONNECTIONS`] is the whole point (spec 017 US5-5, Q7): sharing one pool
/// let a hook burst hold every client slot and lock new windows out of the
/// server. Sixteen concurrent one-shot dispatches is far above the steady-state
/// depth, and an over-cap transient is dropped rather than queued so a burst can
/// never grow unbounded work.
const MAX_TRANSIENT_CONNECTIONS: usize = 16;

/// Admission cap on accepted local connections that have not yet sent a first
/// frame, reserved the instant the stream is accepted — before the handler is
/// spawned — so a flood of silent dialers cannot spawn unbounded tasks (mirrors
/// [`REMOTE_PENDING_HANDSHAKE_CAP`]). Which established pool a connection belongs
/// to is unknowable until its first frame arrives, so this is the only cap the
/// accept loop can charge; it sits generously above both established pools so
/// in-flight handshakes never starve them, and every permit is released within
/// [`LOCAL_PRE_HELLO_TIMEOUT`] at the latest.
const LOCAL_PENDING_CAP: usize = 64;

/// How long an accepted local connection may stay silent before its first frame
/// (spec 017 US5-5, Q7). Every local caller writes its first frame immediately
/// after connecting, so a connection still silent after five seconds is a
/// half-open or abandoned dialer holding a pending admission slot. Reads AFTER
/// the first frame stay untimed — an idle window is legitimate — and remote
/// connections keep their own [`REMOTE_IDLE_READ_TIMEOUT`].
const LOCAL_PRE_HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Bound a one-shot agent call if the foreground client never reports action completion.
const AGENT_ACTION_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

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

/// The write side of one client connection: the enqueue handle into that
/// connection's bounded output queue, drained by the connection's dedicated
/// writer task ([`output_queue_drain`]).
///
/// EVERY connection — local Unix socket as well as remote TCP — goes through the
/// queue. Writing a local socket inline used to let a stopped or merely slow
/// client back-pressure whichever `pty_reader_task` was fanning out to it, which
/// wedges the PTY and, because the authoritative `Term` is shared, freezes every
/// other viewer of that session with it. Enqueueing is a few `VecDeque`
/// operations under a std mutex and never awaits, so no output path can block on
/// a consumer.
///
/// The `Mutex` in [`SharedWriter`] is the connection's ownership token, not an
/// I/O lock: its critical section is one non-blocking enqueue, never an await on
/// the socket.
pub struct ClientSink(OutputSink);

impl ClientSink {
    /// Wrap a connection's enqueue handle as its client sink.
    pub(crate) fn new(sink: OutputSink) -> Self {
        Self(sink)
    }

    /// Clone the lock-free enqueue handle so a session's attached-sink set can
    /// fan output out without ever taking this connection's mutex (and therefore
    /// without holding the per-session set's lock across an await).
    pub(crate) fn queue(&self) -> OutputSink {
        self.0.clone()
    }
}

/// Shared writer half of a client connection.
pub type SharedWriter = Arc<Mutex<ClientSink>>;

/// Delivery state of one attached sink for one session — the buffering sink
/// state machine that closes the attach snapshot/install gap.
///
/// A sink is installed **before** its replay snapshot is taken. Until the replay
/// has been written it is `Buffering`: every sink-bound frame accumulates in
/// emission order instead of racing the replay onto the wire. Once the replay is
/// out, frames the snapshot already reflects are dropped, the rest are flushed in
/// emission order, and the sink flips to `Live`.
enum SinkState {
    /// Frames emitted since the sink was installed, awaiting the replay.
    Buffering(BufferedFrames),
    /// Steady state: frames go straight into the connection's output queue.
    Live,
}

/// Sink-bound frames held while a sink waits for its attach replay.
#[derive(Default)]
struct BufferedFrames {
    frames: VecDeque<BufferedFrame>,
    bytes: usize,
    /// Set when the backlog outgrew [`OUTPUT_QUEUE_PTY_BYTES`] and was dropped.
    /// The flush then asks the connection's writer task for a fresh full replay
    /// instead of handing the client a truncated backlog.
    overflowed: bool,
}

/// One buffered frame plus the Term-commit cursor value that decides whether the
/// attach snapshot already contains its effect.
struct BufferedFrame {
    /// `Some(commit)` for frames whose effect the Term — and therefore the replay
    /// snapshot — also carries (`PtyOutput`, `TrimScrollback`, `ScrollBottom`);
    /// such a frame is dropped when the snapshot was taken at or after `commit`.
    /// `None` for frames no snapshot can carry (metadata, exit, workspace
    /// naming): those always flush.
    commit: Option<u64>,
    msg: ServerMessage,
}

/// One attached sink and its per-session delivery state.
struct AttachedSink {
    /// The connection's `SharedWriter`, kept purely as the `Arc::ptr_eq` identity
    /// token attach/detach match on.
    writer: SharedWriter,
    /// Lock-free enqueue handle into that connection's output queue.
    queue: OutputSink,
    state: SinkState,
    /// Set while this sink's image scene is not known to match the session's:
    /// it just attached, or its queued output was shed. Live image records are
    /// suppressed until a combined replay lands, exactly as `PtyOutput` is
    /// suppressed while its session owes a text resync — an incremental delta
    /// applied to an unknown scene is what produces a divergent viewer.
    owes_image_replay: bool,
}

/// The set of client sinks attached to one session (feature 015 T007). Folds the
/// pre-015 single `Option<SharedWriter>` slot into a fan-out set so N participants
/// receive the same live output; with one attached sink — every legacy /
/// `SingleController` flow — it is the old single slot plus its buffering state.
/// Targeted per-sink sends still address one `SharedWriter` directly via
/// [`send_message`].
#[derive(Default)]
pub struct AttachedSinks {
    sinks: Vec<AttachedSink>,
}

impl AttachedSinks {
    /// Whether any sink is attached (replaces the pre-015 `Option::is_some`).
    pub(crate) fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    /// Seed the set with a single `Live` sink — the creating client of a fresh
    /// session, which has no replay to race and so needs no buffering. Reattach
    /// goes through [`AttachedSinks::begin_attach`] instead.
    fn set_sole(&mut self, writer: SharedWriter, queue: OutputSink) {
        self.sinks.clear();
        // A freshly created session has no scene, so this sink's empty scene
        // already matches and it owes no replay.
        self.sinks.push(AttachedSink {
            writer,
            queue,
            state: SinkState::Live,
            owes_image_replay: false,
        });
    }

    /// Install a sink that still owes an attach replay. Frames emitted from here
    /// on are buffered against it until [`AttachedSinks::finish_attach`] runs, so
    /// nothing emitted between the snapshot and the sink install is lost (#3).
    ///
    /// `additive` picks the feature-015 shared-mode join (keep the other
    /// participants' sinks) over the `SingleController` / legacy takeover
    /// re-point (replace the set).
    fn begin_attach(&mut self, writer: &SharedWriter, queue: OutputSink, additive: bool) {
        if !additive {
            self.sinks.clear();
        }
        self.sinks.retain(|s| !Arc::ptr_eq(&s.writer, writer));
        self.sinks.push(AttachedSink {
            writer: Arc::clone(writer),
            queue,
            state: SinkState::Buffering(BufferedFrames::default()),
            // An attaching sink knows nothing about this session's image scene
            // until the combined replay hands it one.
            owes_image_replay: true,
        });
    }

    /// Release a buffering sink once its replay is on the wire: drop the frames
    /// the snapshot at `snapshot_commit` already reflects, flush the rest in
    /// emission order, and flip the sink to `Live`.
    ///
    /// A backlog that overflowed while buffering resyncs instead — the sink's
    /// connection is marked replay-dirty for `session_id`, so its writer task
    /// sends a fresh full replay that supersedes whatever was shed.
    fn finish_attach(
        &mut self,
        writer: &SharedWriter,
        snapshot_commit: u64,
        session_id: SessionId,
    ) {
        let Some(sink) = self.sinks.iter_mut().find(|s| Arc::ptr_eq(&s.writer, writer)) else {
            return;
        };
        let SinkState::Buffering(buffered) = std::mem::replace(&mut sink.state, SinkState::Live)
        else {
            return;
        };
        if buffered.overflowed {
            sink.queue.mark_dirty(session_id);
            return;
        }
        for frame in buffered.frames {
            if frame.commit.is_some_and(|commit| commit <= snapshot_commit) {
                continue;
            }
            sink.queue.enqueue(&frame.msg);
        }
    }

    /// Fan one sink-bound frame out to every attached sink: straight into the
    /// connection's output queue for a `Live` sink, into the pending buffer for
    /// one still awaiting its replay.
    ///
    /// `commit` is the Term-commit cursor value that includes this frame's effect,
    /// or `None` for frames a replay snapshot cannot carry. Every step is
    /// non-blocking, which is what lets the caller hold the per-session set's lock
    /// across the whole fan-out without ever awaiting under it.
    fn fan_out(&mut self, commit: Option<u64>, msg: &ServerMessage) {
        for sink in &mut self.sinks {
            deliver_frame(sink, commit, msg);
        }
    }

    /// Fan typed image records out to the sinks that can actually render them.
    ///
    /// An incapable sink is skipped rather than sent records it would reject:
    /// the same set can legitimately hold one capable viewer and one incapable
    /// diagnostic connection, and the capable ones must still converge. Returns
    /// how many sinks received the burst, which is the viewer count the
    /// zero/one/multiple-viewer contract is written against.
    ///
    /// A live record is session-scoped and droppable: a saturated link sheds it
    /// with that session's `PtyOutput`, marks the sink replay-dirty, and the
    /// combined replay supersedes everything shed.
    // @lat: [[terminal-images#Terminal Images#Capable-Sink Image Fanout]]
    fn fan_out_images(
        &mut self,
        session_id: SessionId,
        required: TerminalImageCapabilities,
        messages: &[ServerMessage],
    ) -> usize {
        let mut delivered = 0;
        for sink in &mut self.sinks {
            if !sink.queue.image_capabilities().supports(required) {
                continue;
            }
            if sink.owes_image_replay {
                // Suppressed: a delta against an unknown scene diverges.
                continue;
            }
            delivered += 1;
            // A shed frame supersedes the rest of the burst too, so the sink
            // stops here and waits for a combined replay.
            sink.owes_image_replay =
                !messages.iter().all(|msg| deliver_session_frame(sink, session_id, msg));
        }
        delivered
    }

    /// Which attached sinks currently owe a combined image replay.
    ///
    /// The caller plans ONE burst for all of them, which is what keeps the
    /// server from ever holding a per-sink copy of the scene.
    fn image_replay_debt(&self, required: TerminalImageCapabilities) -> usize {
        self.sinks
            .iter()
            .filter(|sink| {
                sink.owes_image_replay && sink.queue.image_capabilities().supports(required)
            })
            .count()
    }

    /// Deliver one planned replay burst to every capable sink that owes one and
    /// clear its debt, returning how many received it.
    ///
    /// Replay records stay on the `Keep` lane, but only while the whole burst
    /// fits that sink's remaining Keep budget. An oversized scene is replaced
    /// atomically by the bounded empty-scene replay plus its typed rejection.
    // @lat: [[terminal-images#Terminal Images#Combined Image Replay]]
    fn fan_out_image_replay(
        &mut self,
        required: TerminalImageCapabilities,
        records: &[ServerMessage],
        degraded: &[ServerMessage],
    ) -> usize {
        let mut delivered = 0;
        for sink in &mut self.sinks {
            if !sink.owes_image_replay || !sink.queue.image_capabilities().supports(required) {
                continue;
            }
            let queued = match &mut sink.state {
                SinkState::Live => sink.queue.enqueue_image_replay(records, degraded),
                SinkState::Buffering(buffered) => buffer_image_replay(buffered, records),
            };
            if queued {
                delivered += 1;
                sink.owes_image_replay = false;
            }
        }
        delivered
    }

    /// Detach the sink identified by `writer` (`Arc::ptr_eq`); returns whether it
    /// was present. The Arc identity token is the same guard `detach_sessions`
    /// applied to the old single slot, so a stale disconnect cannot drop a newer
    /// sink.
    pub(crate) fn detach(&mut self, writer: &SharedWriter) -> bool {
        let before = self.sinks.len();
        self.sinks.retain(|s| !Arc::ptr_eq(&s.writer, writer));
        self.sinks.len() != before
    }
}

/// Hold a replay burst for a sink still awaiting its attach replay, reporting
/// whether the whole burst survived that buffer's own bound. An overflowed
/// buffer is discarded wholesale, so the sink must keep owing the replay: the
/// resync `finish_attach` asks for carries text, not the image scene.
fn buffer_image_replay(buffered: &mut BufferedFrames, records: &[ServerMessage]) -> bool {
    for msg in records {
        buffered.push(None, msg);
    }
    !buffered.overflowed
}

/// Deliver one frame to a sink according to its attach state: straight into the
/// connection's queue when live, into the pending buffer while it still awaits
/// its replay.
fn deliver_frame(sink: &mut AttachedSink, commit: Option<u64>, msg: &ServerMessage) {
    match &mut sink.state {
        SinkState::Live => {
            sink.queue.enqueue(msg);
        }
        SinkState::Buffering(buffered) => buffered.push(commit, msg),
    }
}

/// Deliver one session-scoped droppable frame, reporting whether it is actually
/// on its way to the client.
///
/// `false` means the frame was superseded: the connection's backlog for this
/// session was shed, or a fresh replay is already pending for it. Either way the
/// sink now needs a full scene rather than this delta.
fn deliver_session_frame(
    sink: &mut AttachedSink,
    session_id: SessionId,
    msg: &ServerMessage,
) -> bool {
    match &mut sink.state {
        SinkState::Live => sink.queue.enqueue_session_frame(session_id, msg),
        SinkState::Buffering(buffered) => {
            buffered.push(None, msg);
            // An overflowed attach buffer is resynced by `finish_attach`, and
            // the sink already owes a replay while it buffers.
            !buffered.overflowed
        }
    }
}

impl BufferedFrames {
    /// Append one frame, shedding the whole backlog if it would outgrow the
    /// per-connection output budget. Shedding is safe because the flush turns it
    /// into a fresh full replay; keeping an unbounded buffer would not be.
    fn push(&mut self, commit: Option<u64>, msg: &ServerMessage) {
        if self.overflowed {
            return;
        }
        let bytes = out_frame_bytes(msg);
        if self.bytes.saturating_add(bytes) > OUTPUT_QUEUE_PTY_BYTES {
            warn!(
                queued_bytes = self.bytes,
                "attach buffer overflowed while awaiting replay; resyncing the sink instead"
            );
            self.frames.clear();
            self.bytes = 0;
            self.overflowed = true;
            return;
        }
        self.bytes += bytes;
        self.frames.push_back(BufferedFrame { commit, msg: msg.clone() });
    }
}

/// The per-session set of attached client sinks. A **std** mutex: every critical
/// section is non-blocking bookkeeping plus a queue enqueue, so the compiler
/// enforces that no sink send is ever awaited while the set is locked (#58).
/// Empty when the session is detached (the reader silently skips sends).
pub type ClientWriter = Arc<std::sync::Mutex<AttachedSinks>>;

/// Server-owned image capability for one session, shared by the PTY reader
/// (replies, fan-out) and the dispatch path (attach admission, kill switch).
/// A **std** mutex for the same reason as [`ClientWriter`]: every critical
/// section is non-blocking bookkeeping and never spans an `.await`.
pub type SharedImageSharing = Arc<std::sync::Mutex<SessionImageSharing>>;

/// One session's authoritative terminal-image seam, shared between its PTY
/// reader and its [`LiveSession`] registry entry.
///
/// The reader is the only mutator; the registry handle exists so a hot reload
/// can export the committed scene without asking the reader to cooperate — it
/// is parked on a `read()` at that moment and would never answer. A **tokio**
/// mutex because the reader holds the seam across the awaited ingress feed.
// @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
pub type SessionImageState = Arc<Mutex<PtyTerminalImageState>>;

/// Lock one session's image capability.
pub fn lock_image_sharing(
    sharing: &SharedImageSharing,
) -> std::sync::MutexGuard<'_, SessionImageSharing> {
    sharing.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Lock the per-session attached-sink set.
///
/// The only holders do panic-free `Vec`/`VecDeque` work, so poisoning cannot
/// occur in practice; recover rather than propagate if it somehow does.
pub fn lock_sinks(client_writer: &ClientWriter) -> std::sync::MutexGuard<'_, AttachedSinks> {
    client_writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Build a `SharedWriter` over `write_half`, backed by its own output queue and
/// drain task, for tests that read the resulting frames off a socket pair.
#[cfg(test)]
pub fn test_shared_writer<W>(write_half: W) -> SharedWriter
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sink, _drain) = spawn_output_queue(write_half, new_live_session_registry());
    Arc::new(Mutex::new(ClientSink::new(sink)))
}

/// Monotonic per-session cursor over output the shared `Term` has consumed.
///
/// A PTY chunk advances it by its byte count; a non-byte Term mutation the client
/// mirrors through a frame of its own (a scrollback trim) advances it by one. The
/// counter is read and written ONLY while the session's `Term` mutex is held, so
/// "cursor ≥ C" and "the Term reflects everything tagged ≤ C" are the same
/// statement — which is what lets an attach snapshot decide, without a second
/// critical section, which buffered frames it already contains.
#[derive(Default)]
pub struct TermCommit(AtomicU64);

impl TermCommit {
    /// Read the cursor. Everyone except the owning PTY reader must hold the
    /// `Term` lock; the reader is the sole writer, so its own reads are exact
    /// off-lock.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Advance the cursor past an applied Term mutation, returning the new value.
    /// Call only under the `Term` lock.
    fn advance(&self, delta: u64) -> u64 {
        self.0.fetch_add(delta, Ordering::Relaxed).saturating_add(delta)
    }
}

/// A byte chunk's contribution to the [`TermCommit`] cursor.
fn chunk_len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// Shared handle on a session's [`TermCommit`] cursor, held by the PTY reader
/// (the sole writer) and read by the attach path under the `Term` lock.
pub type SessionCommit = Arc<TermCommit>;

/// Session IDs currently attached to a specific client connection.
pub type AttachedSessionIds = Arc<Mutex<HashSet<SessionId>>>;

/// Shared pointer from a live session to the current attached-session set for
/// its active client, if any.
pub type SessionAttachment = Arc<Mutex<Option<AttachedSessionIds>>>;

/// Server-wide registry of all running sessions. Shared across client
/// handlers and the handoff listener — sessions survive client disconnects.
///
/// **Lock order (spec 017 US1-1):** acquire this before the
/// `workspace_manager` guard, never the other way round, and hold neither
/// across an `.await`. A path that needs a workspace read first — `CloseWindow`
/// resolving a window's session ids — must release it before it touches the
/// registry, so the two never overlap in the opposite direction. The close
/// protocol this exists for is documented in [`crate::session_exit`].
pub type LiveSessionRegistry = Arc<RwLock<HashMap<SessionId, LiveSession>>>;

/// Feature 015 (T006, D1): the single per-window share registry that replaces the
/// three pre-015 per-window maps (`connected_clients`, `window_controllers`,
/// `window_clipboard_gating`) and their `WindowOwnership` tri-lock. Every
/// roster / control / gating / grid mutation for a window is one write-lock
/// acquisition on this map, so the fixed lock order and its drift hazard cease to
/// exist. In the default `SingleController` mode a window's share holds exactly
/// one participant (the legacy writer), so all derived state is byte-identical to
/// feature 013.
pub type WindowShares = Arc<RwLock<HashMap<WindowId, WindowShare>>>;

/// Dismissed CI head per repository. A different published head clears the entry.
pub type CiDismissals = Arc<RwLock<HashMap<PathBuf, String>>>;

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

    /// The `(device_name, login_name)` pair for a roster [`ParticipantInfo`]
    /// (feature 015 T022). A local participant is named like `window_taken_over`
    /// ("this machine", ""); the `is_local` flag carries the real distinction.
    fn participant_naming(&self) -> (String, String) {
        match self {
            ControllerIdentity::Local => ("this machine".to_string(), String::new()),
            ControllerIdentity::Remote { device_name, login_name } => {
                (device_name.clone(), login_name.clone())
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

/// Feature 015 (T006): a server-monotonic participant identifier, stable for a
/// connection's lifetime and used as the `WindowShare.participants` key plus the
/// roster / control-grant target.
pub type ParticipantId = u64;

/// Allocate the next process-wide monotonic [`ParticipantId`]. One counter for
/// the whole server suffices — ids only need to be unique across live
/// participants, and the space never wraps in practice.
fn allocate_participant_id() -> ParticipantId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Feature 015: how a participant reached the share, for roster / audit and
/// eject-on-revoke (data-model Participant.transport). The finer tailnet-vs-LAN
/// split lands with the audit / eject consumers (US3 / T033); until then a remote
/// participant is a single `Remote` bucket, matching what the claim path knows at
/// construction (`ControllerIdentity` local vs remote).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParticipantTransport {
    /// The owning machine's local Unix-socket client.
    Local,
    /// An authenticated remote peer (tailnet or LAN).
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentApiCapability {
    Unsupported,
    Supported,
}

impl From<bool> for AgentApiCapability {
    fn from(supported: bool) -> Self {
        if supported { Self::Supported } else { Self::Unsupported }
    }
}

/// Feature 015 (T006, D1): one attached machine's membership in a window share.
/// Absorbs the per-connection state feature 013 spread across the three retired
/// per-window maps plus the existing per-connection `OutputSink` queue (carried on
/// `writer`).
pub struct Participant {
    /// Stable id for this connection's lifetime; roster + control-grant target.
    pub id: ParticipantId,
    /// The connection's write sink and identity token: `Arc::ptr_eq` on this Arc
    /// is the ownership guard preserved from feature 013, and it is the fan-out
    /// sink: its `ClientSink` enqueues into the connection's output queue.
    pub writer: SharedWriter,
    /// `Local` (owner) or `Remote { device, login }`, for the window-list
    /// controller field and the `WindowTakenOver` naming.
    pub identity: ControllerIdentity,
    /// How the participant reached the share; drives the participant-limit count
    /// (remote joins only) and, later, audit / eject-on-revoke.
    pub transport: ParticipantTransport,
    /// The participant's last-reported terminal viewport (D3); feeds the
    /// smallest-wins grid (T014). Ungated `Resize` reports update it in shared
    /// modes.
    pub viewport: TerminalSize,
    /// Per-participant OSC 52 clipboard-gating capability (spec 010 C7), moved off
    /// the retired per-window `window_clipboard_gating` map (D7).
    pub clipboard_gating: bool,
    /// Whether this participant can decode CI run-state server frames.
    pub ci_run_bar: bool,
    /// Whether this participant can decode agent activity and prompt frames.
    agent_api: AgentApiCapability,
}

impl Participant {
    /// Construct a participant with a fresh id and an empty viewport.
    fn new(
        writer: &SharedWriter,
        identity: ControllerIdentity,
        transport: ParticipantTransport,
        clipboard_gating: bool,
        agent_api: AgentApiCapability,
    ) -> Self {
        Self {
            id: allocate_participant_id(),
            writer: Arc::clone(writer),
            identity,
            transport,
            viewport: TerminalSize::default(),
            clipboard_gating,
            ci_run_bar: false,
            agent_api,
        }
    }

    /// The local owning-machine participant (Unix-socket client).
    fn local(writer: &SharedWriter, clipboard_gating: bool) -> Self {
        Self::new(
            writer,
            ControllerIdentity::Local,
            ParticipantTransport::Local,
            clipboard_gating,
            AgentApiCapability::Unsupported,
        )
    }

    /// Build the participant a `Hello` claim registers: its controller identity,
    /// transport, and advertised clipboard-gating bit.
    fn from_claim(claim: &HelloClaim<'_>, writer: &SharedWriter) -> Self {
        let transport = match claim.controller {
            ControllerIdentity::Local => ParticipantTransport::Local,
            ControllerIdentity::Remote { .. } => ParticipantTransport::Remote,
        };
        let mut participant = Self::new(
            writer,
            claim.controller.clone(),
            transport,
            claim.clipboard_gating,
            claim.agent_api,
        );
        participant.ci_run_bar = claim.ci_run_bar;
        participant
    }
}

/// Feature 015 (T006, D2): who may type in a window, per mode. Replaces the
/// implicit single-writer holder that feature 013 derived from `connected_clients`.
pub enum ControlState {
    /// `SingleController` mode — legacy 013 exclusive ownership. The writer Arc IS
    /// the holder (`Arc::ptr_eq`), identical to feature 013.
    LegacyExclusive { writer: SharedWriter },
    /// Shared view, single typist. At most one holder; `None` = unheld / claimable.
    /// `pending_request` is the in-flight request-and-grant request (US2).
    SingleTypist { holder: Option<ParticipantId>, pending_request: Option<PendingRequest> },
    /// Collaborative free-for-all. Every attached participant may type; no
    /// distinguished holder.
    FreeForAll,
}

/// Feature 015 (T018): an in-flight request-and-grant control request awaiting the
/// approver's (current holder, or owner) decision. Only ever `Some` under
/// `control_acquisition = RequestAndGrant`.
pub struct PendingRequest {
    /// The participant asking for control (the `ControlGrant` target).
    pub requester: ParticipantId,
}

/// Feature 015 (T006, D3): the one terminal grid the session's PTY runs at, sized
/// smallest-wins across attached viewports (FR-012). The debounce coalesces
/// near-simultaneous reports so a settled change drives one `TIOCSWINSZ` (T014).
pub struct AuthoritativeGrid {
    pub rows: u16,
    pub cols: u16,
    pub debounce: std::time::Duration,
    /// Monotonic id of the newest accepted viewport report. The armed timer
    /// compares it across its sleep to tell "the reports stopped" from "another
    /// report landed while I slept".
    report_generation: u64,
    /// Whether a trailing-apply timer is already counting down for this window.
    /// Reports that land while one is armed ride it instead of arming their own.
    apply_armed: bool,
}

impl Default for AuthoritativeGrid {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            debounce: std::time::Duration::from_millis(250),
            report_generation: 0,
            apply_armed: false,
        }
    }
}

impl AuthoritativeGrid {
    /// Record one accepted viewport report and decide whether it has to arm the
    /// trailing-apply timer. Returns the debounce window plus the generation the
    /// armed timer must observe, or `None` when a timer is already counting down —
    /// that timer re-reads the settled viewports, so a burst of reports drives one
    /// apply instead of one per report (#24).
    fn arm_trailing_apply(&mut self) -> Option<(std::time::Duration, u64)> {
        self.report_generation = self.report_generation.wrapping_add(1);
        if self.apply_armed {
            return None;
        }
        self.apply_armed = true;
        Some((self.debounce, self.report_generation))
    }

    /// Resolve one elapsed debounce window for the armed timer. `None` means the
    /// reports settled — the timer disarms and applies once. `Some(generation)`
    /// means a newer report landed during the sleep, so the timer stays armed and
    /// waits out another window observing that generation.
    fn settle_trailing_apply(&mut self, observed: u64) -> Option<u64> {
        if self.report_generation == observed {
            self.apply_armed = false;
            return None;
        }
        Some(self.report_generation)
    }
}

/// Feature 015 (T006, D1): the full state of one window shared across machines —
/// the single source of truth that replaces the three retired per-window maps and
/// their fixed lock order.
pub struct WindowShare {
    /// Attached machines, keyed by id; always ≥1 (the current controller).
    pub participants: HashMap<ParticipantId, Participant>,
    /// Who may type, per mode.
    pub control: ControlState,
    /// The smallest-wins authoritative grid (FR-012). Driven by T014.
    pub grid: AuthoritativeGrid,
    /// Snapshot of `RemoteConfig.sharing_mode` at last mutation.
    pub mode: scribe_config::SharingMode,
    /// Snapshot of `control_acquisition`; only meaningful in single-typist mode —
    /// selects the free-claim vs request-and-grant hand-off policy (T018).
    pub control_acquisition: scribe_config::ControlAcquisition,
    /// Snapshot of `participant_limit`; `None` = unlimited (FR-018). Enforced at
    /// the join by T011.
    pub participant_limit: Option<u32>,
}

impl WindowShare {
    /// Build a share holding one participant, with the initial `ControlState` for
    /// `mode` (D2): `LegacyExclusive` for `SingleController`, a `SingleTypist` held
    /// by the creating participant for `SharedSingleTypist` (US1 keeps input with
    /// the machine that opened the window until US2 wires hand-off), and
    /// `FreeForAll` for free-for-all.
    fn new(
        participant: Participant,
        mode: scribe_config::SharingMode,
        control_acquisition: scribe_config::ControlAcquisition,
        participant_limit: Option<u32>,
    ) -> Self {
        let writer = Arc::clone(&participant.writer);
        let control = match mode {
            scribe_config::SharingMode::SingleController => {
                ControlState::LegacyExclusive { writer }
            }
            scribe_config::SharingMode::SharedSingleTypist => {
                ControlState::SingleTypist { holder: Some(participant.id), pending_request: None }
            }
            scribe_config::SharingMode::FreeForAll => ControlState::FreeForAll,
        };
        let mut participants = HashMap::with_capacity(1);
        participants.insert(participant.id, participant);
        Self {
            participants,
            control,
            grid: AuthoritativeGrid::default(),
            mode,
            control_acquisition,
            participant_limit,
        }
    }

    /// Build a `SingleController` share holding exactly one participant — the
    /// legacy writer — with `LegacyExclusive` control (the legacy-parity and
    /// takeover shape).
    fn new_single_controller(participant: Participant) -> Self {
        Self::new(
            participant,
            scribe_config::SharingMode::SingleController,
            scribe_config::ControlAcquisition::FreeClaim,
            None,
        )
    }

    /// Register an additional participant in this share (feature 015 T010, the
    /// additive join, FR-002). No control change, no disturbance to existing
    /// participants.
    fn add_participant(&mut self, participant: Participant) {
        self.participants.insert(participant.id, participant);
    }

    /// Remove the participant whose sink is `writer` (`Arc::ptr_eq`) — a shared-mode
    /// viewer or holder leaving (feature 015 T010/T019). Returns the removed
    /// participant's id. Holder loss (FR-016): if the departing participant held
    /// `SingleTypist` control, control becomes unheld (`holder = None`) — no silent
    /// inheritance. A `pending_request` is cleared if its requester left.
    fn remove_participant_by_writer(&mut self, writer: &SharedWriter) -> Option<ParticipantId> {
        let id = self
            .participants
            .iter()
            .find(|(_, p)| Arc::ptr_eq(&p.writer, writer))
            .map(|(id, _)| *id)?;
        self.participants.remove(&id);
        if let ControlState::SingleTypist { holder, pending_request } = &mut self.control {
            let holder_left = *holder == Some(id);
            if holder_left {
                *holder = None;
            }
            // Clear a pending request whose approver (the holder) or requester left
            // (data-model FR-016 / spec Edge Case).
            if holder_left || pending_request.as_ref().is_some_and(|r| r.requester == id) {
                *pending_request = None;
            }
        }
        Some(id)
    }

    /// Count of currently-attached REMOTE participants — the quantity
    /// `participant_limit` bounds (the local owner is exempt, FR-007/FR-018).
    fn remote_participant_count(&self) -> usize {
        self.participants
            .values()
            .filter(|p| matches!(p.transport, ParticipantTransport::Remote))
            .count()
    }

    /// Every participant's writer (takeover displaces them all, T010/FR-003).
    fn all_writers(&self) -> Vec<SharedWriter> {
        self.participants.values().map(|p| Arc::clone(&p.writer)).collect()
    }

    /// Writers that advertised support for CI run-state frames in `Hello`.
    fn ci_run_writers(&self) -> Vec<SharedWriter> {
        self.participants
            .values()
            .filter(|participant| participant.ci_run_bar)
            .map(|participant| Arc::clone(&participant.writer))
            .collect()
    }

    /// Writers that advertised support for agent activity and prompt frames.
    fn agent_api_writers(&self) -> Vec<SharedWriter> {
        self.participants
            .values()
            .filter(|participant| participant.agent_api == AgentApiCapability::Supported)
            .map(|participant| Arc::clone(&participant.writer))
            .collect()
    }

    /// Owning-machine client eligible to answer an agent capability prompt.
    fn local_agent_api_writer(&self) -> Option<SharedWriter> {
        self.participants
            .values()
            .find(|participant| {
                participant.transport == ParticipantTransport::Local
                    && participant.agent_api == AgentApiCapability::Supported
            })
            .map(|participant| Arc::clone(&participant.writer))
    }

    /// The smallest-wins grid across attached participants that have reported a
    /// viewport (D3, FR-012): `min(rows) × min(cols)`. `None` until at least one
    /// participant has reported.
    fn smallest_viewport(&self) -> Option<(u16, u16)> {
        self.participants
            .values()
            .map(|p| p.viewport)
            .filter(|v| v.has_grid())
            .map(|v| (v.rows, v.cols))
            .reduce(|(ar, ac), (br, bc)| (ar.min(br), ac.min(bc)))
    }

    /// The participant treated as the window's controller for listing / routing:
    /// the `LegacyExclusive` writer's participant, the `SingleTypist` holder, or —
    /// unheld / free-for-all — the local owner, else any participant.
    fn controller_participant(&self) -> Option<&Participant> {
        match &self.control {
            ControlState::LegacyExclusive { writer } => {
                self.participants.values().find(|p| Arc::ptr_eq(&p.writer, writer))
            }
            ControlState::SingleTypist { holder: Some(id), .. } => self.participants.get(id),
            ControlState::SingleTypist { holder: None, .. } | ControlState::FreeForAll => self
                .participants
                .values()
                .find(|p| matches!(p.identity, ControllerIdentity::Local))
                .or_else(|| self.participants.values().next()),
        }
    }

    /// The current controller's write sink (window-list / broadcast / dispatch
    /// routing target).
    fn controller_writer(&self) -> Option<&SharedWriter> {
        self.controller_participant().map(|p| &p.writer)
    }

    /// The current controller's identity (window-list controller field, takeover
    /// naming).
    fn controller_identity(&self) -> Option<&ControllerIdentity> {
        self.controller_participant().map(|p| &p.identity)
    }

    /// The window's OSC 52 clipboard-gating bit: the current controller's
    /// per-participant capability (byte-identical to the retired per-window map in
    /// `SingleController` mode).
    fn clipboard_gating(&self) -> bool {
        self.controller_participant().is_some_and(|p| p.clipboard_gating)
    }

    /// Whether `writer` is still the window's registered controller — the feature
    /// 013 `Arc::ptr_eq` identity-token guard, now resolved through the share.
    fn is_controlled_by(&self, writer: &SharedWriter) -> bool {
        self.controller_writer().is_some_and(|current| Arc::ptr_eq(current, writer))
    }

    /// The attached participant whose sink is `writer` (`Arc::ptr_eq`), if any.
    fn participant_for_writer(&self, writer: &SharedWriter) -> Option<&Participant> {
        self.participants.values().find(|p| Arc::ptr_eq(&p.writer, writer))
    }

    /// The attached participant whose sink is `writer`, mutably.
    fn participant_for_writer_mut(&mut self, writer: &SharedWriter) -> Option<&mut Participant> {
        self.participants.values_mut().find(|p| Arc::ptr_eq(&p.writer, writer))
    }

    /// The participant id whose sink is `writer` (`Arc::ptr_eq`), if attached.
    fn participant_id_for_writer(&self, writer: &SharedWriter) -> Option<ParticipantId> {
        self.participant_for_writer(writer).map(|p| p.id)
    }

    /// The current `SingleTypist` holder id — the roster `holder` field. `None` for
    /// `LegacyExclusive`, `FreeForAll`, or an unheld single-typist share.
    fn holder_id(&self) -> Option<ParticipantId> {
        match &self.control {
            ControlState::SingleTypist { holder, .. } => *holder,
            _ => None,
        }
    }

    /// The local owning-machine participant, if present.
    fn local_participant(&self) -> Option<&Participant> {
        self.participants.values().find(|p| matches!(p.identity, ControllerIdentity::Local))
    }

    /// Whether `writer`'s departure ends the whole share: the `LegacyExclusive`
    /// writer in `SingleController`, or the local owner in a shared mode. A remote
    /// holder/viewer leaving only removes itself (feature 015 T019).
    fn is_owner_connection(&self, writer: &SharedWriter) -> bool {
        match &self.control {
            ControlState::LegacyExclusive { writer: w } => Arc::ptr_eq(w, writer),
            ControlState::SingleTypist { .. } | ControlState::FreeForAll => {
                self.local_participant().is_some_and(|p| Arc::ptr_eq(&p.writer, writer))
            }
        }
    }

    /// The full-state roster payload for a `ShareRoster` broadcast (feature 015
    /// T022): every participant with its identity naming and `is_local` /
    /// `is_holder` flags, ordered by (monotonic) participant id ≈ join order.
    fn roster(&self) -> Vec<ParticipantInfo> {
        let holder = self.holder_id();
        let mut roster: Vec<ParticipantInfo> = self
            .participants
            .values()
            .map(|p| {
                let (device_name, login_name) = p.identity.participant_naming();
                ParticipantInfo {
                    participant_id: p.id,
                    device_name,
                    login_name,
                    is_local: matches!(p.identity, ControllerIdentity::Local),
                    is_holder: holder == Some(p.id),
                }
            })
            .collect();
        roster.sort_by_key(|p| p.participant_id);
        roster
    }

    /// One participant's roster entry (feature 015 T017/T022) — the `from` field of
    /// a `ControlRequested`.
    fn participant_info(&self, id: ParticipantId) -> Option<ParticipantInfo> {
        let p = self.participants.get(&id)?;
        let (device_name, login_name) = p.identity.participant_naming();
        Some(ParticipantInfo {
            participant_id: p.id,
            device_name,
            login_name,
            is_local: matches!(p.identity, ControllerIdentity::Local),
            is_holder: self.holder_id() == Some(p.id),
        })
    }

    /// The remote participants' controller info for the window-list occupancy
    /// enrichment (feature 015 T022/T026), ordered by device then login.
    fn remote_controller_infos(&self) -> Vec<ControllerInfo> {
        let mut infos: Vec<ControllerInfo> =
            self.participants.values().filter_map(|p| p.identity.to_controller_info()).collect();
        infos.sort_by(|a, b| {
            a.device_name.cmp(&b.device_name).then_with(|| a.login_name.cmp(&b.login_name))
        });
        infos
    }
}

/// Agent world-capture view (spec 027). Implemented against the library's
/// trait via `crate::agent_api` in both compiles of this file — the binary
/// re-exports the library's `agent_api`, so its recompiled `WindowShare`
/// still satisfies the one nominal bound `agent_api::world::capture` uses.
impl crate::agent_api::world::ShareView for WindowShare {
    fn sharing_mode(&self) -> scribe_config::SharingMode {
        self.mode
    }

    fn participant_count(&self) -> usize {
        self.participants.len()
    }
}

#[derive(Clone)]
pub struct IpcServerState {
    pub session_manager: Arc<SessionManager>,
    pub workspace_manager: Arc<RwLock<WorkspaceManager>>,
    /// Last-good Beads snapshots, shared by every window on this server.
    pub beads_boards: BeadsBoardCache,
    pub live_sessions: LiveSessionRegistry,
    /// Feature 015 (T006, D1): the single per-window share registry that replaces
    /// the three retired per-window maps (`connected_clients`,
    /// `window_clipboard_gating`, `window_controllers`) and their tri-lock. Holds
    /// the participant set, control state, clipboard-gating bits, and grid for
    /// every connected window; in `SingleController` mode each share carries one
    /// participant, so all derived state is byte-identical to feature 013.
    pub window_shares: WindowShares,
    /// Current dismissed CI head per repo, shared across every attached window.
    pub ci_dismissals: CiDismissals,
    /// Existing tracker loop; detail interest reuses its auth and scheduler.
    pub github_ci_tracker: GithubCiTrackerHandle,
    pub updater_handle: Arc<UpdaterHandle>,
    /// In-memory cache of GitHub releases populated lazily on the first
    /// `ListReleases` request and refreshed in the background past its TTL.
    /// See [`crate::releases`] for the cache state machine.
    pub release_catalog: Arc<Mutex<ReleaseCatalog>>,
    /// Fetcher used to refresh `release_catalog`. Production wires the real
    /// `GithubReleaseFetcher`; tests may inject deterministic stubs.
    pub release_fetcher: Arc<dyn ReleaseFetcher>,
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
    /// Server-wide local-push detector, absent internally while its setting is off.
    pub git_ref_watcher: Arc<GitRefWatcherControl>,
    /// Bounded one-shot request router for the local agent control surface.
    pub agent_api: crate::agent_api::AgentApiState,
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
    ai_launch: Option<AiLaunchSpec>,
    shell_tool: Option<ShellTool>,
    env_envelope_id: Option<String>,
}

#[derive(Clone, Copy)]
struct SessionRuntimeContext<'a> {
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
    live_sessions: &'a LiveSessionRegistry,
    git_ref_watcher: &'a Arc<GitRefWatcherControl>,
    /// Feature 015 (T006): the per-window share registry, consulted by the PTY
    /// reader for the controller's spec-010 clipboard-gating bit.
    window_shares: &'a WindowShares,
}

#[derive(Clone, Copy)]
pub struct MetadataRuntime<'a> {
    pub workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
    pub live_sessions: &'a LiveSessionRegistry,
    pub window_shares: &'a WindowShares,
    pub git_ref_watcher: &'a Arc<GitRefWatcherControl>,
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
    /// Shared exit funnel (spec 017 US1-3). The reader arbitrates its own
    /// EOF/read-error exit against explicit closes and the child-exit watcher
    /// through this gate's CAS.
    exit_gate: Arc<SessionExitGate>,
    /// Cancellation arm raced against the PTY read. The master fd is
    /// duplicated into the resize fd and the `LiveSession`'s `Pty`, so a
    /// SIGHUP-trapping child would otherwise park this task on a `read()`
    /// that can never EOF.
    cancel: CancelWaiter,
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    /// Monotonic cursor over what `term` has consumed. Advanced in the same
    /// `Term` critical section as every feed, so an attach can pair a snapshot
    /// with the exact output it contains.
    term_commit: SessionCommit,
    /// Exactly one authoritative terminal-image seam for this session. The
    /// reader is its only mutator; the handle is shared with [`LiveSession`]
    /// so the handoff serializer can export the committed scene.
    terminal_images: SessionImageState,
    /// This session's latched image capability and master-switch state.
    image_sharing: SharedImageSharing,
    /// The find overlay's cached scrollback snapshot, dropped by every feed
    /// this task performs (spec 017 US8-2).
    search_cache: SessionSearchCache,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
    workspace_manager: Arc<RwLock<WorkspaceManager>>,
    live_sessions: LiveSessionRegistry,
    git_ref_watcher: Arc<GitRefWatcherControl>,
    /// Feature 015 (T006): the per-window share registry, read for the window
    /// controller's spec-010 clipboard-gating capability bit.
    window_shares: WindowShares,
    /// Per-session OSC 52 gating state (spec 010 E3). Wave 2 only uses
    /// `outstanding_prompt` + `policy`; the burst-reuse fields land later.
    //
    // @lat: [[server#Sessions#Clipboard Gating]]
    clipboard_burst: ClipboardBurstState,
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
    /// Whether a client's last focus report named this session as focused,
    /// shared with [`LiveSession`]. Read when the application enables focus
    /// reporting so it learns the state it missed.
    has_focus: Arc<AtomicBool>,
    /// `FOCUS_IN_OUT` as of the previous chunk, so the reader can deliver the
    /// current focus state exactly once per DECSET 1004 enable.
    focus_mode_was_active: bool,
    /// Duplicate-redraw trim baseline for the current AI scrollback epoch.
    preserved_ai_scrollback: PreservedAiScrollback,
    /// Waiting for the first filtered redraw in the epoch to commit.
    pending_ai_scrollback_baseline: bool,
    /// Last emitted application-image evidence, so the summary line is written
    /// once per real change instead of once per PTY read.
    image_evidence: ImageApplicationEvidence,
}

/// What a real terminal application's graphics have done to this session.
///
/// The cumulative counters come from the observed boundaries of every
/// committed read, so they survive the erase or scroll that retires a
/// placement; the placement counts are the live canonical snapshot, which is
/// what names Kitty classic, Kitty Unicode-placeholder, and Sixel apart.
// @lat: [[terminal-images#Terminal Images#Pinned Application Corpus]]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ImageApplicationEvidence {
    /// Replies written back to the PTY (Kitty results and augmented DA).
    replies: u64,
    /// Kitty commands observed, including queries and continuations.
    kitty_commands: u64,
    /// Kitty commands that completed a transfer with decoded canonical facts.
    kitty_transfers: u64,
    /// Sixel images decoded from DCS payloads.
    sixel_images: u64,
    /// Typed graphics failures raised by framing, decode, or storage.
    failures: u64,
    /// Live canonical placements by kind.
    classic_placements: usize,
    placeholder_placements: usize,
    sixel_placements: usize,
}

/// Minimum spacing between two grid applies driven by one session's `Resize`
/// stream (spec 017 US7-3) — 250 ms, so a continuous drag settles to at most
/// four applies per second. Matches the shared-window debounce window.
const RESIZE_APPLY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// What a `Resize` report has to do once [`ResizePacer`] has admitted it.
enum ResizeAdmission {
    /// Nothing has been applied inside the current window: apply now.
    ApplyNow,
    /// Too soon after the last apply and no timer is counting down. The caller
    /// arms one for this delay; it applies whatever is pending when it fires.
    Arm(std::time::Duration),
    /// A timer is already counting down and will pick this report up.
    Coalesced,
}

/// Rate limiter over one session's controller-driven grid applies (spec 017
/// US7-3). A drag republishes a pane's grid every frame, and each report used to
/// drive a full `Term` reflow plus a `TIOCSWINSZ` — so the reflows ran at event
/// rate and the child paid a `SIGWINCH` per step. Applies now run leading-edge
/// and then no more often than [`RESIZE_APPLY_INTERVAL`], with the newest report
/// held as the trailing one so the drag always lands on the size it stopped at.
#[derive(Default)]
struct ResizePacer {
    /// When the last apply ran; `None` until the first report, which is why an
    /// isolated resize is never delayed.
    last_apply: Option<std::time::Instant>,
    /// Newest size reported since that apply, waiting on the armed timer.
    pending: Option<TerminalSize>,
    /// Whether a trailing-apply task is already counting down for this session.
    armed: bool,
}

impl ResizePacer {
    /// Admit one report and decide who applies it.
    fn admit(&mut self, size: TerminalSize, now: std::time::Instant) -> ResizeAdmission {
        if self.armed {
            self.pending = Some(size);
            return ResizeAdmission::Coalesced;
        }
        let waited = self.last_apply.map(|last| now.duration_since(last));
        match waited {
            Some(waited) if waited < RESIZE_APPLY_INTERVAL => {
                self.pending = Some(size);
                self.armed = true;
                ResizeAdmission::Arm(RESIZE_APPLY_INTERVAL.saturating_sub(waited))
            }
            _ => {
                self.last_apply = Some(now);
                self.pending = None;
                ResizeAdmission::ApplyNow
            }
        }
    }

    /// Disarm the trailing timer and take the size it owes an apply. `None` only
    /// when the pending report was already consumed, which leaves the pacer idle
    /// rather than holding a timer no report will ever mature.
    fn take_pending(&mut self, now: std::time::Instant) -> Option<TerminalSize> {
        self.armed = false;
        let size = self.pending.take()?;
        self.last_apply = Some(now);
        Some(size)
    }

    /// Drop the size an armed timer is still holding, without recording an
    /// apply. The last client detaching calls this: a report from a gesture
    /// that ended with the connection has no one to serve, and — the real
    /// hazard — it would otherwise mature *after* the next client's
    /// attach-time resize and reinstate the pre-detach grid.
    ///
    /// `armed` deliberately stays set: the in-flight task still owns the
    /// disarm, and leaving it set keeps a report admitted before it fires from
    /// arming a second, overlapping timer. That task simply finds nothing to
    /// apply.
    fn discard_pending(&mut self) {
        self.pending = None;
    }

    /// Record a grid apply that bypassed the pacer — the attach-time resize and
    /// the shared-window authoritative grid both drive `resize_term` directly.
    ///
    /// Dropping `pending` is what makes the newest applied size the one that
    /// wins: a timer armed before this apply can no longer reinstate the size
    /// it was holding. Stamping `last_apply` folds the direct apply into the
    /// pacing window too, so the first report after it is spaced like any other
    /// rather than costing an immediate extra reflow.
    fn note_external_apply(&mut self, now: std::time::Instant) {
        self.pending = None;
        self.last_apply = Some(now);
    }
}

/// Lock a session's [`ResizePacer`], recovering a poisoned mutex the way
/// [`lock_sinks`] does. The pacer is three plain fields with no invariant a
/// panicking holder can leave half-applied, and the critical sections never
/// await, so the lock is never held across a suspension point.
fn lock_resize_pacer(
    pacer: &std::sync::Mutex<ResizePacer>,
) -> std::sync::MutexGuard<'_, ResizePacer> {
    pacer.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A running session in the server-wide registry. Lives independently of
/// any client connection — the `client_writer` is set/cleared as clients
/// attach and detach.
pub struct LiveSession {
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    resize_fd: Arc<OwnedFd>,
    pub(crate) term:
        Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    /// Monotonic cursor over what `term` has consumed — read under the `Term`
    /// lock alongside a replay snapshot so the attach knows which buffered
    /// frames that snapshot already reflects.
    pub(crate) term_commit: SessionCommit,
    /// Payload-free Alacritty observation shared with the PTY reader and every
    /// production resize call for this session.
    terminal_grid_observer: TerminalGridObserverHandle,
    /// The reader's authoritative image seam, reachable here so a hot reload
    /// can export this session's committed scene, paused framing, and
    /// in-flight transfer into the successor's payload.
    terminal_images: SessionImageState,
    /// Scrollback snapshot the open find overlay's query edits are reading
    /// (spec 017 US8-2), so a multi-keystroke query snapshots once.
    search_cache: SessionSearchCache,
    child_pid: u32,
    /// Per-boot identity token for `child_pid` (spec 017 US7-2). Checked
    /// before [`signal_if_handoff_session`] hangs the child up, and forwarded
    /// to the next server on handoff.
    child_identity: Option<crate::child_identity::ChildIdentity>,
    /// `pub(crate)` so `hook_ingress` can clone the writer when routing
    /// inbound `HookEvent`s through `send_metadata_event`.
    pub(crate) client_writer: ClientWriter,
    /// Server-owned image capability, latched independently of viewer count so
    /// detach, reattach, and controller changes cannot change what the running
    /// application was already told.
    pub(crate) image_sharing: SharedImageSharing,
    attachment: SessionAttachment,
    workspace_id: WorkspaceId,
    shell_name: String,
    /// Last-known terminal title (OSC 0/2), persisted for reconnect.
    title: Option<String>,
    /// Last-known icon/tab title (OSC 0/1), persisted for reconnect.
    icon_title: Option<String>,
    /// Last-known provider task label, persisted separately from OSC 0/2 titles.
    task_label: Option<String>,
    /// Last-known working directory (OSC 7), persisted for reconnect.
    cwd: Option<std::path::PathBuf>,
    /// Last CWD this server process pushed through the metadata pipeline.
    /// Kept separate from `cwd` (which is restored from a handoff) so the
    /// first report after a handoff still reaches clients that only ever
    /// saw the previous process.
    last_cwd_report: Option<std::path::PathBuf>,
    /// Memoized `.git/HEAD` walk for the directory the session is in, shared
    /// by the metadata pipeline and `ListSessions`.
    git_branch_cache: GitBranchCache,
    /// Last-known remote/tmux context reported by shell integration.
    context: Option<scribe_common::protocol::SessionContext>,
    /// Last-known AI process state (OSC 1337), persisted for reconnect.
    ai_state: Option<scribe_common::ai_state::AiProcessState>,
    /// Launch-time AI provider hint used when the session CLI does not emit
    /// explicit provider metadata.
    ai_provider_hint: Option<AiProvider>,
    /// Launch-only tool identity retained for `SessionList` and handoff restore.
    shell_tool: Option<ShellTool>,
    /// Prompt history for the running conversation, retained alongside
    /// `ai_state` so a client that attaches after the prompts were submitted
    /// still gets a prompt bar. Cleared with `ai_state`: the provider exiting
    /// ends the conversation the history belongs to.
    prompt_state: Option<scribe_common::protocol::SessionPromptState>,
    /// The Beads issue the live agent is currently working on. This is
    /// ephemeral session state: hook ingress owns writes, and teardown clears
    /// it rather than persisting it through reconnect or handoff.
    focused_issue: Option<String>,
    /// Latest known terminal cell size in pixels.
    cell_width: u16,
    cell_height: u16,
    /// Paces controller-driven grid applies for this session (spec 017 US7-3),
    /// so a drag's report stream costs at most four reflows per second.
    ///
    /// Behind its own mutex rather than reached through the registry's write
    /// lock: detach and the unpaced direct applies both have to reach it while
    /// holding only a shared reference to the session.
    resize_pacer: std::sync::Mutex<ResizePacer>,
    /// Keep the Pty alive so the child process isn't killed by SIGHUP on Drop.
    /// `None` for sessions restored from a hot-reload handoff. Taken by the
    /// close paths, which route the SIGHUP + `waitpid` off-worker through
    /// [`PtyGuard::teardown`], and by `defuse_for_handoff`, which leaks it via
    /// [`PtyGuard::defuse`] so a hot-reload never hangs up on a child.
    pty: Option<PtyGuard>,
    /// Screen snapshot from a hot-reload handoff, sent to the first client
    /// that attaches. Taken (cleared) after first use.
    pub(crate) handoff_snapshot: Option<scribe_common::screen::ScreenSnapshot>,
    /// Shared runtime flag updated by config reloads.
    preserve_ai_scrollback: Arc<AtomicBool>,
    /// Shared runtime scrollback limit updated by config reloads.
    scrollback_lines: Arc<AtomicUsize>,
    /// Whether a client's last focus report named this session as focused.
    /// Shared with the reader task, which delivers the state to the PTY when
    /// the application enables focus reporting (DECSET 1004).
    has_focus: Arc<AtomicBool>,
    /// The window that requested this session at create time, or the same
    /// stable owner carried through handoff. Stashed on the session itself
    /// (rather than re-derived from the workspace manager) so the clean-close
    /// path can route the env-envelope delete after the session→window mapping
    /// has been torn down. Stable for the session's lifetime.
    ///
    /// `pub(crate)` so [`crate::hook_ingress`] can read it when routing an
    /// `EnvChanged` event into [`crate::env_store::EnvStoreState::schedule_persist`].
    pub(crate) env_window_id: WindowId,
    /// Launch-record id (== env-envelope id) naming this session's
    /// `<state_dir>/restore/env/<window_id>/<launch_id>.envz` file plus
    /// its keystore DEK. Carried by `CreateSession.env_envelope_id`: every
    /// create path mints one, so this is `Some` from creation for anything a
    /// client asked for, and handoff carries it into the successor. `None`
    /// remains possible for legacy handoffs and pre-minting clients; hook
    /// ingress fills the latter on the session's first persistable delta.
    ///
    /// `pub(crate)` so [`crate::hook_ingress`] can read and bootstrap it when
    /// routing an `EnvChanged` event into
    /// [`crate::env_store::EnvStoreState::schedule_persist`].
    pub(crate) env_envelope_id: Option<String>,
    /// Sender into the PTY reader task's OSC 52 control channel (spec 010
    /// C4). The client message dispatcher forwards
    /// `ClipboardPromptResponse` and `ClipboardBridgeReadReply` here so
    /// they reach `handle_clipboard_prompt_response` /
    /// `handle_clipboard_bridge_read_reply` on the owning reader task.
    pub(crate) clipboard_command_tx: tokio::sync::mpsc::UnboundedSender<ClipboardCommand>,
    /// Shared exit funnel (spec 017 US1-3): the reader's cancellation signal,
    /// its retained `JoinHandle`, and the CAS that elects the one path allowed
    /// to publish `SessionExited` and unwire this session.
    exit_gate: Arc<SessionExitGate>,
    /// This session's reservation against the global session cap (spec 017
    /// US7-1), carried over from its [`ManagedSession`]. Never read — the
    /// registry entry owning it *is* the accounting, and its `Drop` returns
    /// the slot the moment the entry leaves the map on any close path.
    _slot: SessionSlot,
}

/// The per-session handles the exit funnel needs, cloned off a [`LiveSession`]
/// before a close path drops it. Keeping them out of the registry means the
/// finalizer can run after the session value is gone.
struct SessionExitHandles {
    exit_gate: Arc<SessionExitGate>,
    client_writer: ClientWriter,
    attachment: SessionAttachment,
}

pub struct AttachSessionData {
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub shell_name: String,
    pub client_writer: ClientWriter,
    pub attachment: SessionAttachment,
    pub term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    pub term_commit: SessionCommit,
    pub terminal_grid_observer: TerminalGridObserverHandle,
    /// The session's canonical image seam and its capability latch, so a fresh
    /// sink can be paid its replay debt at attach instead of on the next
    /// committed PTY read.
    pub terminal_images: SessionImageState,
    pub image_sharing: SharedImageSharing,
    pub resize_fd: Arc<OwnedFd>,
    pub target_dims: Option<TerminalSize>,
    pub has_handoff_snapshot: bool,
    /// The session's exit funnel, so an attach that queued behind the replay
    /// concurrency cap can tell whether the session it targets died while it
    /// waited.
    pub exit_gate: Arc<SessionExitGate>,
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
            self.terminal_grid_observer.set_cell_size(size.cell_width, size.cell_height);
        }

        AttachSessionData {
            session_id,
            workspace_id: self.workspace_id,
            shell_name: self.shell_name.clone(),
            client_writer: Arc::clone(&self.client_writer),
            attachment: Arc::clone(&self.attachment),
            term: Arc::clone(&self.term),
            term_commit: Arc::clone(&self.term_commit),
            terminal_grid_observer: self.terminal_grid_observer.clone(),
            terminal_images: Arc::clone(&self.terminal_images),
            image_sharing: Arc::clone(&self.image_sharing),
            resize_fd: Arc::clone(&self.resize_fd),
            target_dims,
            has_handoff_snapshot: self.handoff_snapshot.is_some(),
            exit_gate: Arc::clone(&self.exit_gate),
        }
    }

    pub fn take_handoff_snapshot(&mut self) -> Option<ScreenSnapshot> {
        self.handoff_snapshot.take()
    }

    /// Take the session's PTY guard so a close path can tear it down after it
    /// has released every lock. `None` for handoff-restored sessions, which
    /// [`signal_if_handoff_session`] hangs up on explicitly instead.
    fn take_pty(&mut self) -> Option<PtyGuard> {
        self.pty.take()
    }

    /// Clone the handles a close path needs to drive the exit funnel after the
    /// session value itself has been dropped.
    fn exit_handles(&self) -> SessionExitHandles {
        SessionExitHandles {
            exit_gate: Arc::clone(&self.exit_gate),
            client_writer: Arc::clone(&self.client_writer),
            attachment: Arc::clone(&self.attachment),
        }
    }
}

/// Closure shape produced by `alacritty_terminal` for OSC 52 read replies.
/// Defined in [`crate::clipboard_state`] so [`crate::clipboard_state::DeferredRequest`]
/// can hold a parked formatter while a burst-deferred read waits on a prompt.
use crate::clipboard_state::{ClipboardBurstState, ClipboardReplyFormatter};

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

/// The local (Unix-socket) admission pools. Three independent semaphores so no
/// class of connection can starve another (spec 017 US5-5): `pending` bounds
/// accepted-but-unclassified connections, and a connection's first frame moves it
/// into exactly one of `client` (long-lived, holds a window) or `transient`
/// (one-shot, holds nothing) for the rest of its life.
struct LocalAdmission {
    pending: Arc<tokio::sync::Semaphore>,
    client: Arc<tokio::sync::Semaphore>,
    transient: Arc<tokio::sync::Semaphore>,
}

impl LocalAdmission {
    fn new() -> Self {
        Self {
            pending: Arc::new(tokio::sync::Semaphore::new(LOCAL_PENDING_CAP)),
            client: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
            transient: Arc::new(tokio::sync::Semaphore::new(MAX_TRANSIENT_CONNECTIONS)),
        }
    }
}

/// Which established pool a classified local connection belongs to.
#[derive(Clone, Copy)]
enum LocalSlotKind {
    /// Claims a window and lives until the client disconnects.
    Client,
    /// Answers at most one frame and closes, registering nothing.
    Transient,
}

/// One local connection's admission slot: the pre-first-frame `pending` permit
/// taken in the accept loop, exchanged exactly once for the established permit
/// its first frame calls for. Dropped when the connection handler returns, which
/// is what returns the slot to its pool.
struct LocalSlot {
    pools: Arc<LocalAdmission>,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl LocalSlot {
    /// Exchange the held pending permit for an established-pool permit. The new
    /// permit is taken BEFORE the old one is released (the assignment drops it),
    /// so the exchange can never transiently over-admit either pool. Returns
    /// `false` when the target pool is full and the caller must close.
    fn claim(&mut self, kind: LocalSlotKind) -> bool {
        let pool = match kind {
            LocalSlotKind::Client => &self.pools.client,
            LocalSlotKind::Transient => &self.pools.transient,
        };
        let Ok(permit) = Arc::clone(pool).try_acquire_owned() else {
            return false;
        };
        self.permit = permit;
        true
    }
}

/// Start the IPC accept loop on an already-bound listener.
pub async fn start_ipc_server(
    listener: UnixListener,
    server: IpcServerState,
) -> Result<(), ScribeError> {
    let admission = Arc::new(LocalAdmission::new());

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                if !verify_peer_uid(&stream) {
                    continue;
                }

                // Admission is two-stage (spec 017 US5-5). The accept loop can
                // only charge the pending pool, because a connection's class is
                // not knowable until its first frame arrives;
                // `establish_client_window` exchanges this permit for a client or
                // transient one, and a dialer that never sends is dropped after
                // `LOCAL_PRE_HELLO_TIMEOUT`.
                let Ok(permit) = Arc::clone(&admission.pending).try_acquire_owned() else {
                    warn!("pending-connection limit ({LOCAL_PENDING_CAP}) reached, rejecting");
                    continue;
                };
                let slot = LocalSlot { pools: Arc::clone(&admission), permit };

                info!("client connected");
                let server = server.clone();
                tokio::spawn(async move {
                    // `Box::pin` the connection future so it lives on the heap
                    // rather than bloating this spawned task's stack frame.
                    Box::pin(handle_client(stream, server, slot)).await;
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

/// Per-connection cap on queued droppable `PtyOutput` payload bytes
/// (feature 013 T029, research D5, PR-004). When the backlog would exceed this
/// the whole `PtyOutput` queue is shed and its sessions are marked replay-dirty
/// for a fresh full replay, so a stalled consumer's memory stays bounded without
/// ever back-pressuring the PTY. Control/replay frames are never counted here.
const OUTPUT_QUEUE_PTY_BYTES: usize = 4 * 1024 * 1024;

/// Per-connection cap on TOTAL queued bytes across every frame kind —
/// including the non-droppable `Keep` lane (`SessionReplay`, `SessionCreated`,
/// `TitleChanged`, `ClipboardBridgeWrite`, …). Prevents an unbounded `Keep`
/// backlog on a stalled link: on breach the droppable `PtyOutput` backlog is shed
/// first, and if the queue is STILL over ceiling (a pure control-frame flood the
/// link cannot drain) the connection is closed so its memory use stays bounded
/// (FR-013). Sits above [`OUTPUT_QUEUE_PTY_BYTES`] to leave headroom for a
/// legitimate multi-session initial attach replay.
const OUTPUT_QUEUE_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// Per-connection cap on the number of queued frames, bounding a flood of
/// small `Keep` control frames the byte ceiling would under-count. Same
/// shed-then-close policy as [`OUTPUT_QUEUE_TOTAL_BYTES`].
const OUTPUT_QUEUE_MAX_FRAMES: usize = 8192;

/// Nominal byte cost charged to a queued control frame whose exact serialized
/// size is not worth computing. High-volume streams and legacy per-cell
/// `ScreenSnapshot` frames are sized precisely; every other frame is small, so a
/// flat nominal keeps accounting cheap while the frame ceiling backstops floods.
const OUTPUT_FRAME_NOMINAL_BYTES: usize = 256;

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
const REMOTE_IDLE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(30);

/// Idle time before the OS starts sending TCP keepalive probes on an accepted
/// remote connection, and the interval between probes. With the OS-default probe
/// count this drops a vanished (FIN/RST-less) peer — and frees its authorized
/// slot — in a few minutes instead of waiting out [`REMOTE_IDLE_READ_TIMEOUT`],
/// with no false positives on a live-but-idle viewer (a live TCP stack ACKs the
/// probes even when the app sends nothing).
const REMOTE_KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_mins(1);
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

// ── Bounded per-connection output queue (feature 013 T029, generalized) ──────
//
// A stalled consumer must never block the fan-out hot path (research D5,
// FR-013). If `send_pty_output` / `send_to_client` wrote straight to a stalled
// socket, the owning session's `pty_reader_task` would wedge on the write,
// back-pressuring the PTY and freezing the running program — and, because the
// authoritative `Term` is shared, stalling every other client with it. So EVERY
// connection — local Unix socket as well as remote TCP — interposes this bounded
// queue: its `SharedWriter` only enqueues (never awaits the socket), and a single
// drain task owns the write half. A SIGSTOP'd or merely slow local client is
// exactly as harmless as a slow tailnet link.
//
// When queued `PtyOutput` would exceed the cap the whole backlog is dropped and
// its sessions are marked replay-dirty; the drain task then sends each a fresh
// full `SessionReplay` once the consumer drains (catch-up-to-current, the tmux
// `%pause`→`capture-pane` model).

/// Enqueue handle into a connection's bounded output queue. Cloneable and
/// cheap: every `SharedWriter` clone for the connection (the window-registry slot
/// and each attached session's `client_writer`) holds one, all funneling into the
/// single queue drained by [`output_queue_drain`].
#[derive(Clone)]
pub struct OutputSink(Arc<OutputQueueShared>);

/// Shared state between a connection's [`OutputSink`] producers and its
/// one drain task. The queue is guarded by a *std* mutex: every critical section
/// is a few non-blocking `VecDeque` / `HashSet` operations and never spans an
/// `.await`, so producers never park on the link (and the `!Send` guard makes the
/// compiler enforce that discipline in the spawned drain task).
struct OutputQueueShared {
    inner: std::sync::Mutex<OutputQueueInner>,
    /// Wakes the drain task when frames are enqueued, a session goes replay-dirty,
    /// or the connection is shutting down.
    notify: tokio::sync::Notify,
    /// This connection's `Hello` image-renderer capability, packed by
    /// [`pack_image_capabilities`]. It lives here rather than behind the
    /// connection's async mutex because the image fan-out runs inside the
    /// per-session sink lock, where nothing may await.
    image_capabilities: std::sync::atomic::AtomicU32,
    /// Whether this connection can deserialize structured Pi provider metadata.
    pi_provider: AtomicBool,
}

/// Pack a capability set into one atomic word: the low bits are feature bits,
/// the sign bit is runtime enablement.
const IMAGE_RUNTIME_ENABLED_BIT: u32 = 1 << 31;

fn pack_image_capabilities(capabilities: TerminalImageCapabilities) -> u32 {
    let mut packed = u32::from(capabilities.features.bits());
    if capabilities.runtime_enabled {
        packed |= IMAGE_RUNTIME_ENABLED_BIT;
    }
    packed
}

fn unpack_image_capabilities(packed: u32) -> TerminalImageCapabilities {
    TerminalImageCapabilities {
        runtime_enabled: packed & IMAGE_RUNTIME_ENABLED_BIT != 0,
        features: scribe_common::terminal_images::TerminalImageFeatures::from_bits(
            u16::try_from(packed & u32::from(u16::MAX)).unwrap_or(0),
        ),
    }
}

struct OutputQueueInner {
    /// Frames awaiting the link, in send order.
    frames: VecDeque<OutFrame>,
    /// Running total of queued droppable `PtyOutput` payload bytes — the quantity
    /// the overflow cap ([`OUTPUT_QUEUE_PTY_BYTES`]) governs.
    queued_pty_bytes: usize,
    /// Running total of queued bytes across EVERY frame kind (droppable and
    /// `Keep`), governed by [`OUTPUT_QUEUE_TOTAL_BYTES`] so an unbounded
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

/// One queued frame. Only [`OutFrame::Session`] is droppable by the overflow
/// policy: it carries a session's high-volume incremental streams — raw
/// `PtyOutput` and the typed live image records committed alongside it — whose
/// loss a fresh combined replay repairs. Every other message (takeover notice,
/// session-exit, workspace update, an attach or resync replay, …) is kept so it
/// is never silently lost. Each variant carries its accounted byte size so both
/// the droppable cap and the total-queue cap can be maintained in O(1) on pop.
#[derive(Clone)]
enum OutFrame {
    Session { session_id: SessionId, bytes: usize, msg: ServerMessage },
    Keep { bytes: usize, msg: ServerMessage },
}

fn keep_frames(messages: &[ServerMessage]) -> Vec<OutFrame> {
    messages
        .iter()
        .map(|msg| OutFrame::Keep { bytes: out_frame_bytes(msg), msg: msg.clone() })
        .collect()
}

fn keep_batch_cost(messages: &[ServerMessage]) -> usize {
    messages.iter().fold(0usize, |total, msg| total.saturating_add(out_frame_bytes(msg)))
}

fn batch_can_ever_fit(bytes: usize, frames: usize) -> bool {
    bytes <= OUTPUT_QUEUE_TOTAL_BYTES && frames <= OUTPUT_QUEUE_MAX_FRAMES
}

/// Make room for an already-bounded Keep batch by shedding only droppable
/// backlog. Returns false without mutating the queue when existing Keep frames
/// leave too little room even after that shed.
fn make_keep_batch_fit(g: &mut OutputQueueInner, bytes: usize, frames: usize) -> bool {
    let keep_frames =
        g.frames.iter().filter(|frame| matches!(frame, OutFrame::Keep { .. })).count();
    let keep_bytes = g.queued_total_bytes.saturating_sub(g.queued_pty_bytes);
    if keep_bytes.saturating_add(bytes) > OUTPUT_QUEUE_TOTAL_BYTES
        || keep_frames.saturating_add(frames) > OUTPUT_QUEUE_MAX_FRAMES
    {
        return false;
    }
    if g.queued_total_bytes.saturating_add(bytes) > OUTPUT_QUEUE_TOTAL_BYTES
        || g.frames.len().saturating_add(frames) > OUTPUT_QUEUE_MAX_FRAMES
    {
        drop_pty_backlog(g);
    }
    true
}

impl OutputQueueInner {
    /// Pop the next queued frame's message in send order, decrementing both the
    /// `PtyOutput` byte total and the whole-queue byte total so the overflow caps
    /// track what is still buffered.
    fn pop_message(&mut self) -> Option<ServerMessage> {
        match self.frames.pop_front()? {
            OutFrame::Session { bytes, msg, .. } => {
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

impl OutputQueueShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, OutputQueueInner> {
        // The only lock holders are the enqueue path and the drain task, both
        // doing panic-free `VecDeque` / `HashSet` work, so poisoning cannot occur
        // in practice; recover rather than propagate if it somehow does.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl OutputSink {
    /// Record this connection's advertised image-renderer capability.
    pub(crate) fn set_image_capabilities(&self, capabilities: TerminalImageCapabilities) {
        self.0.image_capabilities.store(pack_image_capabilities(capabilities), Ordering::Relaxed);
    }

    /// Read this connection's advertised image-renderer capability.
    pub(crate) fn image_capabilities(&self) -> TerminalImageCapabilities {
        unpack_image_capabilities(self.0.image_capabilities.load(Ordering::Relaxed))
    }

    /// Record whether this connection can decode structured Pi provider state.
    pub(crate) fn set_pi_provider_capability(&self, supported: bool) {
        self.0.pi_provider.store(supported, Ordering::Relaxed);
    }

    fn pi_compatible_message<'a>(&self, msg: &'a ServerMessage) -> Option<Cow<'a, ServerMessage>> {
        if self.0.pi_provider.load(Ordering::Relaxed) {
            return Some(Cow::Borrowed(msg));
        }
        match msg {
            ServerMessage::AiStateChanged { ai_state, .. }
                if ai_state.provider == AiProvider::Pi =>
            {
                None
            }
            ServerMessage::TaskLabelChanged { provider: AiProvider::Pi, .. }
            | ServerMessage::TaskLabelCleared { provider: AiProvider::Pi, .. }
            | ServerMessage::PromptReceived { provider: AiProvider::Pi, .. } => None,
            ServerMessage::SessionList { .. } => {
                let mut compatible = msg.clone();
                compatible.make_pi_provider_compatible(false);
                Some(Cow::Owned(compatible))
            }
            _ => Some(Cow::Borrowed(msg)),
        }
    }

    /// Enqueue one `ServerMessage` for the drain task. Never blocks on the link.
    ///
    /// `PtyOutput` is the sole droppable, high-volume stream: while its session is
    /// replay-dirty it is suppressed, and when the queued `PtyOutput` backlog would
    /// exceed [`OUTPUT_QUEUE_PTY_BYTES`] the entire backlog is dropped and every
    /// affected session (this one included) is marked replay-dirty. Every other
    /// message is kept in order, but the whole queue is bounded too
    /// ([`OUTPUT_QUEUE_TOTAL_BYTES`] / [`OUTPUT_QUEUE_MAX_FRAMES`]):
    /// on breach the droppable backlog is shed, and a queue still over ceiling is
    /// a hopelessly stalled link, so the connection is closed (bounded resource
    /// use, FR-013).
    ///
    /// The (potentially multi-MB) message clone is built BEFORE the queue lock is
    /// taken, so the std mutex is only ever held for O(1) `VecDeque`/`HashSet`
    /// bookkeeping — never for a large `memcpy`.
    ///
    /// Returns `false` only when the connection is already closed — the enqueue
    /// equivalent of the pre-queue "dead socket" write error.
    fn enqueue(&self, msg: &ServerMessage) -> bool {
        let Some(msg) = self.pi_compatible_message(msg) else {
            return true;
        };
        let msg = msg.as_ref();
        let bytes = out_frame_bytes(msg);
        if let ServerMessage::PtyOutput { session_id, .. } = msg {
            return self.enqueue_droppable(*session_id, bytes, msg).is_some();
        }
        // Clone off the lock (see doc-comment): the frame is prepared here and
        // simply discarded if the queue is already closed.
        let frame = OutFrame::Keep { bytes, msg: msg.clone() };
        let mut g = self.0.lock();
        if g.closed {
            return false;
        }
        g.queued_total_bytes += bytes;
        g.frames.push_back(frame);
        enforce_queue_ceiling(&mut g);
        drop(g);
        self.0.notify.notify_one();
        true
    }

    /// Atomically enqueue one combined image replay without ever exposing a
    /// partial scene or letting an oversized `Keep` burst close the connection.
    ///
    /// A full replay that cannot fit alongside the sink's existing `Keep`
    /// frames is replaced by `degraded`, the two-record empty scene carrying a
    /// typed quota rejection. Droppable backlog does not consume Keep budget —
    /// it is shed to make room before the batch is installed.
    fn enqueue_image_replay(&self, records: &[ServerMessage], degraded: &[ServerMessage]) -> bool {
        if records.is_empty() || degraded.is_empty() {
            return false;
        }
        let full_cost = keep_batch_cost(records);
        let degraded_cost = keep_batch_cost(degraded);
        let full_admissible = batch_can_ever_fit(full_cost, records.len());
        // Clone off the lock, as [`Self::enqueue`] does: both candidate batches
        // are built here so the std mutex only ever does O(1) bookkeeping.
        let mut full_frames = if full_admissible { keep_frames(records) } else { Vec::new() };
        let mut degraded_frames = keep_frames(degraded);

        let mut g = self.0.lock();
        if g.closed {
            return false;
        }
        let (frames, cost, was_degraded) =
            if full_admissible && make_keep_batch_fit(&mut g, full_cost, records.len()) {
                (std::mem::take(&mut full_frames), full_cost, false)
            } else if make_keep_batch_fit(&mut g, degraded_cost, degraded.len()) {
                (std::mem::take(&mut degraded_frames), degraded_cost, true)
            } else {
                return false;
            };

        g.queued_total_bytes = g.queued_total_bytes.saturating_add(cost);
        g.frames.extend(frames);
        enforce_queue_ceiling(&mut g);
        debug_assert!(!g.closed, "a pre-budgeted replay batch must survive the queue ceiling");
        drop(g);
        self.0.notify.notify_one();

        if was_degraded {
            warn!(
                replay_bytes = full_cost,
                replay_frames = records.len(),
                "terminal image replay exceeded the sink Keep budget; sent an empty scene"
            );
        }
        true
    }

    /// Enqueue one session-scoped droppable frame — raw `PtyOutput` or a live
    /// image record — reporting whether it is actually on its way.
    ///
    /// `false` means the frame was superseded rather than queued: the session
    /// already owes a fresh replay, or this frame's arrival shed the backlog.
    /// The image fan-out uses that answer to stop sending deltas and plan a
    /// combined replay instead; the raw output path ignores it, because the
    /// text resync the drain task already owes covers it.
    fn enqueue_session_frame(&self, session_id: SessionId, msg: &ServerMessage) -> bool {
        let bytes = out_frame_bytes(msg);
        self.enqueue_droppable(session_id, bytes, msg) == Some(true)
    }

    /// Shared droppable-frame policy. `None` means the connection is closed;
    /// `Some(false)` means the frame was superseded by a pending replay.
    fn enqueue_droppable(
        &self,
        session_id: SessionId,
        bytes: usize,
        msg: &ServerMessage,
    ) -> Option<bool> {
        // Clone off the lock (see doc-comment): the frame is prepared here and
        // simply discarded if the overflow policy below decides to drop it.
        let frame = OutFrame::Session { session_id, bytes, msg: msg.clone() };
        let mut g = self.0.lock();
        if g.closed {
            return None;
        }
        if g.dirty.contains(&session_id) {
            // A fresh replay is already pending for this session; its live
            // output is superseded — drop it and skip the wakeup.
            return Some(false);
        }
        let queued = if g.queued_pty_bytes.saturating_add(bytes) > OUTPUT_QUEUE_PTY_BYTES {
            // Overflow: shed the whole droppable backlog and let a fresh replay
            // catch every affected session (this one too) back up.
            drop_pty_backlog(&mut g);
            g.dirty.insert(session_id);
            false
        } else {
            g.queued_pty_bytes += bytes;
            g.queued_total_bytes += bytes;
            g.frames.push_back(frame);
            true
        };
        enforce_queue_ceiling(&mut g);
        drop(g);
        self.0.notify.notify_one();
        Some(queued)
    }

    /// Mark `session_id` replay-dirty so the drain task sends it a fresh full
    /// `SessionReplay`. The attach path uses this when a sink's pre-replay buffer
    /// overflowed: catching the client up to current supersedes the shed backlog.
    fn mark_dirty(&self, session_id: SessionId) {
        let mut g = self.0.lock();
        if g.closed {
            return;
        }
        g.dirty.insert(session_id);
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
fn drop_pty_backlog(g: &mut OutputQueueInner) {
    let mut kept = VecDeque::with_capacity(g.frames.len());
    for frame in std::mem::take(&mut g.frames) {
        match frame {
            OutFrame::Session { session_id, .. } => {
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

/// Byte cost charged to a queued frame. High-volume payloads and the legacy
/// per-cell snapshot are sized precisely so the total-queue cap sees their real
/// footprint; small controls use [`OUTPUT_FRAME_NOMINAL_BYTES`].
fn out_frame_bytes(msg: &ServerMessage) -> usize {
    match msg {
        ServerMessage::PtyOutput { data, .. } => data.len(),
        ServerMessage::SessionReplay { replay, .. } => replay.replay_zstd.len(),
        ServerMessage::ScreenSnapshot { .. } => {
            rmp_serde::to_vec_named(msg).map_or(usize::MAX, |encoded| encoded.len())
        }
        // Image records carry canonical RGBA in bounded chunks. Charging them a
        // flat nominal would let a large scene's replay outgrow the queue's
        // byte ceiling without the ceiling ever noticing.
        ServerMessage::TerminalImageLive { message, .. } => live_record_bytes(message),
        ServerMessage::TerminalImageReplay { message, .. } => replay_record_bytes(message),
        _ => OUTPUT_FRAME_NOMINAL_BYTES,
    }
}

/// Payload bytes one live image record puts on the wire.
fn live_record_bytes(message: &scribe_common::terminal_images::TerminalImageLiveMessage) -> usize {
    use scribe_common::terminal_images::{TerminalImageLiveMessage, TerminalImageUpdate};
    match message {
        TerminalImageLiveMessage::Update {
            update: TerminalImageUpdate::DefinitionChunk { chunk },
            ..
        } => OUTPUT_FRAME_NOMINAL_BYTES.saturating_add(chunk.data.len()),
        _ => OUTPUT_FRAME_NOMINAL_BYTES,
    }
}

/// Payload bytes one replay record puts on the wire.
fn replay_record_bytes(
    message: &scribe_common::terminal_images::TerminalImageReplayMessage,
) -> usize {
    use scribe_common::terminal_images::TerminalImageReplayMessage;
    match message {
        TerminalImageReplayMessage::DefinitionChunk { chunk, .. } => {
            OUTPUT_FRAME_NOMINAL_BYTES.saturating_add(chunk.data.len())
        }
        _ => OUTPUT_FRAME_NOMINAL_BYTES,
    }
}

/// Bound total queue memory after an enqueue (feature 013 flow-control hardening,
/// FR-013). When the queue exceeds its total-byte or frame-count ceiling — a
/// stalled link accumulating `Keep` frames the `PtyOutput` cap does not govern —
/// shed the droppable backlog first; if it is STILL over ceiling the link cannot
/// keep up even with output dropped, so mark the connection closed for teardown
/// (the client auto-reconnects to a fresh replay). A no-op on a healthy queue.
fn enforce_queue_ceiling(g: &mut OutputQueueInner) {
    if g.queued_total_bytes <= OUTPUT_QUEUE_TOTAL_BYTES && g.frames.len() <= OUTPUT_QUEUE_MAX_FRAMES
    {
        return;
    }
    drop_pty_backlog(g);
    if g.queued_total_bytes > OUTPUT_QUEUE_TOTAL_BYTES || g.frames.len() > OUTPUT_QUEUE_MAX_FRAMES {
        warn!(
            queued_bytes = g.queued_total_bytes,
            frames = g.frames.len(),
            "output queue over ceiling after shedding backlog; closing stalled connection"
        );
        g.closed = true;
    }
}

/// Build a connection's output queue and spawn its drain task (which owns the
/// write half). Returns the enqueue handle to install into the connection's
/// `SharedWriter` plus the drain task's join handle, awaited (bounded) at
/// teardown by [`shutdown_output_queue`].
///
/// `live_sessions` is the registry the drain task rebuilds a catch-up
/// `SessionReplay` from when a session goes replay-dirty.
pub fn spawn_output_queue<W>(
    write_half: W,
    live_sessions: LiveSessionRegistry,
) -> (OutputSink, tokio::task::JoinHandle<()>)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let shared = Arc::new(OutputQueueShared {
        inner: std::sync::Mutex::new(OutputQueueInner {
            frames: VecDeque::new(),
            queued_pty_bytes: 0,
            queued_total_bytes: 0,
            dirty: HashSet::new(),
            closed: false,
        }),
        notify: tokio::sync::Notify::new(),
        // Incapable until this connection's `Hello` says otherwise.
        image_capabilities: std::sync::atomic::AtomicU32::new(0),
        pi_provider: AtomicBool::new(false),
    });
    let drain = tokio::spawn(output_queue_drain(Arc::clone(&shared), write_half, live_sessions));
    (OutputSink(shared), drain)
}

/// Write one queued frame, reporting whether the connection is still usable.
///
/// A frame that cannot be encoded — most plausibly a `ScreenSnapshot` of a very
/// deep scrollback exceeding [`scribe_common::framing::MAX_MESSAGE_SIZE`] — is
/// dropped with a log line rather than taken as a connection fault: encoding
/// fails before any byte reaches the socket, so the stream is still intact and
/// the next frame is fine. Only a genuine I/O error, which can leave a partial
/// frame on the wire, tears the connection down.
async fn write_queued_frame<W>(write_half: &mut W, msg: &ServerMessage) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match write_message(write_half, msg).await {
        Ok(()) => true,
        Err(ScribeError::Io { source }) => {
            debug!("output queue write failed: {source}");
            false
        }
        Err(e) => {
            warn!("dropping unsendable frame: {e}");
            true
        }
    }
}

/// The single writer for a connection: flush queued frames to the socket, then
/// send a fresh full `SessionReplay` for any replay-dirty session, then park
/// until more work arrives (or the connection closes). This is the ONLY task that
/// writes the socket, so frame order on the wire is exactly enqueue order.
async fn output_queue_drain<W>(
    shared: Arc<OutputQueueShared>,
    mut write_half: W,
    live_sessions: LiveSessionRegistry,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        // 1. Flush every queued frame, in order.
        loop {
            let Some(msg) = shared.lock().pop_message() else { break };
            if !write_queued_frame(&mut write_half, &msg).await {
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
            if !send_resync_replay(&shared, &mut write_half, &live_sessions, session_id).await {
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
    shared: &Arc<OutputQueueShared>,
    write_half: &mut W,
    live_sessions: &LiveSessionRegistry,
    session_id: SessionId,
) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let handles = {
        let sessions = live_sessions.read().await;
        sessions.get(&session_id).map(|s| (Arc::clone(&s.term), Arc::clone(&s.term_commit)))
    };
    let Some((term, term_commit)) = handles else {
        // Session ended while dirty; nothing to replay.
        shared.lock().dirty.remove(&session_id);
        return true;
    };
    let replay = match crate::attach_flow::take_session_replay(
        session_id,
        &term,
        &term_commit,
        live_sessions,
    )
    .await
    {
        Ok((replay, _commit)) => replay,
        Err(e) => {
            warn!(%session_id, "resync replay build failed: {e}");
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
    if !write_queued_frame(write_half, &msg).await {
        shared.lock().closed = true;
        return false;
    }
    true
}

/// Stop a connection's drain task at teardown, bounded so a wedged consumer
/// cannot exceed the disable budget (FR-016). Any already-queued final frame
/// (e.g. the sever `RemoteDisconnect`) flushes first; then the write half drops
/// and the socket closes.
async fn shutdown_output_queue(sink: &OutputSink, mut drain_task: tokio::task::JoinHandle<()>) {
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
    /// Feature 015 (D6): the owning machine's live sharing settings, refreshed
    /// from `RemoteConfig` by the supervisor on every reload and read by the claim
    /// path so a new `WindowShare` is created in the current mode. Behind a std
    /// mutex; every critical section is a trivial copy off the `.await` path. Only
    /// the US1 fields are tracked here; request-and-grant acquisition arrives with
    /// US2.
    sharing: std::sync::Mutex<SharingSnapshot>,
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

/// Feature 015 (D6): the owning machine's sharing settings the claim path reads to
/// build a new `WindowShare` in the current mode. Snapshotted from `RemoteConfig`
/// by the supervisor; defaults keep legacy `SingleController` behavior.
#[derive(Clone, Copy)]
struct SharingSnapshot {
    mode: scribe_config::SharingMode,
    control_acquisition: scribe_config::ControlAcquisition,
    participant_limit: Option<u32>,
}

impl Default for SharingSnapshot {
    fn default() -> Self {
        Self {
            mode: scribe_config::SharingMode::SingleController,
            control_acquisition: scribe_config::ControlAcquisition::FreeClaim,
            participant_limit: None,
        }
    }
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
            sharing: std::sync::Mutex::new(SharingSnapshot::default()),
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

    /// Refresh the sharing snapshot from a reloaded `RemoteConfig` (feature 015 D6).
    /// Called by the supervisor on startup and every reload so the claim path sees
    /// the current mode without a restart (FR-017).
    /// Refresh the sharing snapshot from a reloaded `RemoteConfig`, returning the
    /// PREVIOUS snapshot so the supervisor can detect a mode change and reconcile
    /// active shares (feature 015 T032).
    fn update_sharing(&self, cfg: &scribe_config::RemoteConfig) -> SharingSnapshot {
        let snapshot = SharingSnapshot {
            mode: cfg.sharing_mode,
            control_acquisition: cfg.control_acquisition,
            participant_limit: cfg.participant_limit,
        };
        std::mem::replace(
            &mut self.sharing.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
            snapshot,
        )
    }

    /// The current sharing snapshot the claim path builds a new share from.
    fn sharing_snapshot(&self) -> SharingSnapshot {
        *self.sharing.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
                // Feature 015 (D6): refresh the sharing snapshot so a Hello handled
                // after this reload builds its `WindowShare` in the current mode.
                let previous = control.update_sharing(&cfg.remote);
                // Feature 015 (T032, FR-017): a live mode change reconciles every
                // ACTIVE share immediately (no restart), over this `ConfigReloaded`
                // path — demote / detach / re-broadcast per the data-model table.
                if previous.mode != cfg.remote.sharing_mode {
                    reconcile_shares_for_mode_change(&server, control.sharing_snapshot()).await;
                }
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
/// bounded output queue and a `Remote(device label)` controller. The
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
    let (sink, drain_task) =
        spawn_output_queue(write_half, Arc::clone(&accept.server.live_sessions));
    let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::new(sink.clone())));
    let ctx = RemoteContext {
        node_name: device_name,
        login_name: String::new(),
        audit: RemoteAudit::Lan { device_id_short: short_device_id(&peer.device_id) },
        sever: sever_rx,
    };
    // `serve_connection`/`finish_served_connection` emit the `lan: accepted …` and
    // `lan: disconnect …` audit lines via the `RemoteAudit::Lan` branch.
    serve_connection(reader, writer, accept.server.clone(), Some(ctx), None).await;
    // Flush any final frame (e.g. the sever `RemoteDisconnect`), stop the drain
    // task, then release the sever registration + connection slot.
    shutdown_output_queue(&sink, drain_task).await;
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
    // Snapshot the locally-controlled windows' writers from the share registry
    // under one read lock, then send with no lock held.
    let local_writers: Vec<SharedWriter> = {
        let shares = server.window_shares.read().await;
        shares
            .values()
            .filter_map(WindowShare::controller_participant)
            .filter(|p| matches!(p.identity, ControllerIdentity::Local))
            .map(|p| Arc::clone(&p.writer))
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
                send_handshake_reply(
                    &mut write_half,
                    client_version,
                    Some(RemoteRefusal::Disabled),
                )
                .await;
                info!(
                    target: REMOTE_AUDIT_TARGET,
                    "remote: refused peer={} reason={}",
                    identity.node_name,
                    audit_reason(RemoteRefusal::Disabled)
                );
                // The cap permit drops with this scope.
                return;
            };
            send_handshake_reply(&mut write_half, client_version, None).await;
            // Feature 013 (T029, research D5): interpose a bounded per-connection
            // output queue between the fan-out hot path and this (possibly slow)
            // tailnet link. The drain task owns the TCP write half; the
            // `SharedWriter` only enqueues, so a stalled remote consumer can never
            // block the server's authoritative `Term` or the other clients.
            let (sink, drain_task) =
                spawn_output_queue(write_half, Arc::clone(&server.live_sessions));
            let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::new(sink.clone())));
            let ctx = RemoteContext {
                node_name: identity.node_name,
                login_name: identity.login_name,
                audit: RemoteAudit::Tailnet,
                sever: sever_rx,
            };
            serve_connection(reader, writer, server, Some(ctx), None).await;
            // Flush any final frame (e.g. the sever `RemoteDisconnect`) and stop the
            // drain task before releasing the connection slot.
            shutdown_output_queue(&sink, drain_task).await;
            control.deregister_connection(Transport::Tailnet, conn_id).await;
            // Release the connection-cap slot once the connection is fully done.
            drop(permit);
        }
        Err(reject) => {
            send_handshake_reply(&mut write_half, client_version, Some(reject.refusal)).await;
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
async fn send_handshake_reply<W>(
    writer: &mut W,
    client_version: u32,
    refusal: Option<RemoteRefusal>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let reply = ServerMessage::RemoteHandshakeReply {
        accepted: refusal.is_none(),
        refusal,
        server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        server_scribe_version: env!("CARGO_PKG_VERSION").to_owned(),
        version_mismatch: (refusal == Some(RemoteRefusal::IncompatibleVersion))
            .then(|| {
                scribe_common::terminal_images::RemoteProtocolMismatch::between(
                    client_version,
                    REMOTE_PROTOCOL_VERSION,
                )
            })
            .flatten(),
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

/// The advisory singleton lock, held for the server's whole life.
///
/// An upgrade receiver acquires a new lock after the predecessor acknowledges
/// and exits, closing the former gap where successors ran permanently unlocked.
pub type ServerLock = Option<nix::fcntl::Flock<std::fs::File>>;

fn acquire_server_lock_with(
    argument: nix::fcntl::FlockArg,
) -> Result<nix::fcntl::Flock<std::fs::File>, ScribeError> {
    let lock_path = scribe_common::socket::server_lock_path();
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| ScribeError::Io { source: e })?;

    let lock_file =
        nix::fcntl::Flock::lock(lock_file, argument).map_err(|(_, _)| ScribeError::IpcError {
            reason: "another scribe-server is already running (lock held)".into(),
        })?;
    Ok(lock_file)
}

/// Acquire the process-wide server lock after an acknowledged handoff.
pub fn acquire_server_lock() -> Result<nix::fcntl::Flock<std::fs::File>, ScribeError> {
    acquire_server_lock_with(nix::fcntl::FlockArg::LockExclusive)
}

/// Try to acquire the process-wide server lock without blocking.
pub fn try_acquire_server_lock() -> Result<nix::fcntl::Flock<std::fs::File>, ScribeError> {
    acquire_server_lock_with(nix::fcntl::FlockArg::LockExclusiveNonblock)
}

/// Acquire the server socket with singleton enforcement.
///
/// In normal mode, uses an advisory flock on `server.lock` to serialise
/// the bind-or-connect sequence.  If another server already holds the
/// socket, returns `IpcError` ("already running").  In upgrade mode the
/// lock and liveness check are skipped — the handoff protocol coordinates
/// the two servers, and the old server still holds the lock and serves on
/// the path this call takes over.
///
/// Returns the lock file guard (must be kept alive) and the bound listener.
/// Both callers bind before serving: the normal path from `run_normal_server`,
/// the upgrade path from inside `receive_handoff`, ahead of its ACK.
///
/// # Errors
/// Returns an error when the singleton lock is held, another server answers on
/// the socket, or the bind itself fails. In upgrade mode an error here aborts
/// the handoff before the ACK, leaving the old server serving.
pub fn acquire_server_socket(
    socket_path: &Path,
    upgrade_mode: bool,
) -> Result<(ServerLock, UnixListener), ScribeError> {
    // Ensure the parent directory exists with 0700 permissions.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::Io { source: e })?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ScribeError::Io { source: e })?;
    }

    if upgrade_mode {
        // Upgrade mode: replace the socket atomically. The handoff protocol has
        // already coordinated with the old server, which is still listening on
        // the inode this rename displaces — see `bind_over`.
        //
        // No singleton lock is taken here: the old server holds it until it
        // exits. That is safe precisely because the path is never unowned, so a
        // concurrently launched normal-mode server always finds a live server
        // on the far end and refuses to start rather than racing for the path.
        let listener = bind_over(socket_path)?;
        return Ok((None, listener));
    }

    // Normal mode: acquire flock then bind-or-connect.
    let lock_file = try_acquire_server_lock()?;

    // Try to bind the socket.  If it fails with EADDRINUSE the path
    // already exists; any other error is a real failure.
    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => {
            set_socket_permissions(socket_path);
            listener
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
            try_bind(socket_path)?
        }
        Err(bind_err) => return Err(ScribeError::Io { source: bind_err }),
    };

    Ok((Some(lock_file), listener))
}

/// Bind `socket_path` by binding a sibling staging path and renaming over it.
///
/// `remove_file` followed by `bind` leaves a window in which the path does not
/// exist, and an upgrade receiver takes this path while the old server is still
/// serving on it. The client treats a single failed `connect` as "no server is
/// running" and cold-starts a stateless server through systemd
/// ([[crates/scribe-client/src/server_lifecycle.rs#connect_or_start_server]]),
/// so any window at all is long enough to lose every session the successor is
/// carrying. `rename` is atomic: a concurrent `connect` reaches either the old
/// server or this one, never nothing.
fn bind_over(socket_path: &Path) -> Result<UnixListener, ScribeError> {
    let staging = staging_socket_path(socket_path);
    // A staging file survives only a successor that died between bind and
    // rename; its inode is unreachable either way, so clear it unconditionally.
    drop(std::fs::remove_file(&staging));

    let listener = try_bind(&staging)?;
    if let Err(e) = std::fs::rename(&staging, socket_path) {
        drop(std::fs::remove_file(&staging));
        return Err(ScribeError::Io { source: e });
    }
    Ok(listener)
}

/// Fixed sibling path used to bind before renaming over the live socket. The
/// old server processes one handoff peer through ACK at a time, so only that
/// receiver can reach this path; a fixed name also lets it clear a staging
/// socket left by a receiver that died before rename.
fn staging_socket_path(socket_path: &Path) -> std::path::PathBuf {
    let mut name = socket_path.file_name().unwrap_or_default().to_os_string();
    name.push(".upgrade");
    socket_path.with_file_name(name)
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
///
/// The write half goes to a bounded output queue with its own drain task, the
/// same interposition remote connections use: a SIGSTOP'd or slow local client
/// fills its queue instead of back-pressuring whichever `pty_reader_task` is
/// fanning out to it, so the shared `Term` — and every other session — keeps
/// running.
async fn handle_client(
    stream: tokio::net::UnixStream,
    server: IpcServerState,
    mut slot: LocalSlot,
) {
    let (reader, write_half) = tokio::io::split(stream);
    let (sink, drain_task) = spawn_output_queue(write_half, Arc::clone(&server.live_sessions));
    let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::new(sink.clone())));
    serve_connection(reader, writer, server, None, Some(&mut slot)).await;
    // Flush whatever is still queued, stop the drain task, close the socket.
    shutdown_output_queue(&sink, drain_task).await;
}

/// Drive the post-handshake per-connection protocol over any framed stream —
/// shared verbatim by the local Unix-socket path and the feature-013 remote TCP
/// path. `remote` is `Some` only for accepted remote connections: it gates the
/// transient no-Hello actions off (they are local-socket only) and carries the
/// tailnet identity for the accepted/disconnect audit lines. Local connections
/// (`None`) behave exactly as before.
///
/// `local` is the mirror image: `Some` only for local Unix-socket connections,
/// carrying the admission slot whose pending permit the first frame exchanges for
/// a client or transient one (spec 017 US5-5). Remote connections pass `None` —
/// their caps are charged by their own transport's accept path.
async fn serve_connection<R>(
    mut reader: R,
    writer: SharedWriter,
    server: IpcServerState,
    remote: Option<RemoteContext>,
    local: Option<&mut LocalSlot>,
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

    // Heap-allocated rather than inlined: the establish path carries the whole
    // pre-`Hello` first-frame dispatch, and this call sits inside the long
    // `serve_connection` future, which has to stay under the workspace's
    // future-size budget.
    let Some(window_id) =
        Box::pin(establish_client_window(&mut reader, conn, &controller, sever_rx.as_mut(), local))
            .await
    else {
        // A bare pre-Hello close, a refused admission slot, or remote access was
        // disabled before Hello arrived (T023). No window was claimed, so close.
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

/// Record a connection's renderer capability on its lock-free output-queue
/// handle and return the subset it is told it has.
///
/// The handle rather than the connection mutex, because the image fan-out runs
/// inside the per-session sink lock where nothing may await.
async fn record_image_capabilities(
    writer: &SharedWriter,
    advertised: TerminalImageCapabilities,
) -> TerminalImageCapabilities {
    let images = effective_connection_subset(advertised, images_master_enabled());
    writer.lock().await.queue().set_image_capabilities(images);
    images
}

async fn record_pi_provider_capability(writer: &SharedWriter, supported: bool) {
    writer.lock().await.queue().set_pi_provider_capability(supported);
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
    let severed = matches!(exit, LoopExit::Severed);
    if severed {
        send_remote_disconnect(conn.writer, RemoteRefusal::Disabled).await;
    }

    detach_client_window(window_id, conn.server, conn.attached_ids, conn.writer, severed).await;

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

/// Complete a `Hello` first frame: charge the connection's client slot, record
/// the renderer capability it advertised, and register its window claim.
///
/// `hello` is always [`ClientMessage::Hello`]; any other frame is refused
/// without touching the admission pool.
async fn claim_hello_window(
    hello: ClientMessage,
    conn: ConnState<'_>,
    controller: &ControllerIdentity,
    local: Option<&mut LocalSlot>,
) -> Option<WindowId> {
    let ClientMessage::Hello {
        window_id,
        clipboard_gating,
        takeover,
        join_window,
        terminal_images,
        ci_run_bar,
        pi_provider,
        agent_api,
        ..
    } = hello
    else {
        return None;
    };
    // Long-lived: exchange the pending permit for one of the 32 client slots.
    // A full pool closes the connection, exactly as the pre-017 accept-time
    // rejection did — minus the accept.
    if !claim_local_slot(local, LocalSlotKind::Client) {
        return None;
    }
    record_pi_provider_capability(conn.writer, pi_provider).await;
    let claim = HelloClaim {
        requested_window_id: window_id,
        clipboard_gating,
        intent: if takeover {
            ClaimIntent::Takeover
        } else if join_window {
            ClaimIntent::Join
        } else {
            ClaimIntent::Plain
        },
        controller,
        terminal_images: record_image_capabilities(conn.writer, terminal_images).await,
        ci_run_bar,
        agent_api: agent_api.into(),
    };
    Some(handle_client_hello(claim, conn.server, conn.writer).await)
}

async fn establish_client_window<R>(
    reader: &mut R,
    conn: ConnState<'_>,
    controller: &ControllerIdentity,
    mut sever_rx: Option<&mut tokio::sync::oneshot::Receiver<()>>,
    mut local: Option<&mut LocalSlot>,
) -> Option<WindowId>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let is_remote = matches!(controller, ControllerIdentity::Remote { .. });
    // Remote connections keep their long idle backstop; a local connection gets
    // the tight pre-Hello bound (spec 017 US5-5) because it is still holding a
    // pending admission permit and every local caller writes immediately.
    let first_frame_timeout =
        if is_remote { REMOTE_IDLE_READ_TIMEOUT } else { LOCAL_PRE_HELLO_TIMEOUT };
    loop {
        // Race each pre-Hello read against the sever signal so a disable (T023)
        // closes a remote connection even before it claims a window; the read also
        // carries the pre-Hello read bound so an abandoned dialer frees its slot.
        // Local connections pass no sever channel, so that arm never fires there.
        let first = tokio::select! {
            biased;
            () = await_sever(sever_rx.as_deref_mut()) => {
                debug!("remote access disabled before Hello; closing");
                return None;
            }
            read = read_client_frame(reader, Some(first_frame_timeout)) => read,
        };
        let Some(first) = first else {
            debug!(is_remote, "connection idle before Hello; closing");
            return None;
        };
        match first {
            Ok(hello @ ClientMessage::Hello { .. }) => {
                return claim_hello_window(hello, conn, controller, local.as_deref_mut()).await;
            }
            // Feature 013 (fix 5): the picker's window-probe. An ALREADY-authorized
            // remote connection may enumerate this machine's windows read-only
            // BEFORE `Hello`; reply with `WindowList` and keep reading (the probe
            // then closes, or — tolerated — a `Hello` follows on the same link). No
            // window is registered by a bare `ListWindows`. Every OTHER non-Hello
            // first frame still closes (transient no-Hello actions stay local-only).
            Ok(ClientMessage::ListWindows) if is_remote => {
                handle_list_windows(
                    &conn.server.window_shares,
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
                // Charge the connection to the pool its first frame classifies it
                // into BEFORE any work is dispatched, so a hook burst is bounded
                // by its own semaphore instead of the client slots (US5-5).
                let kind = if is_transient_first_frame(&msg) {
                    LocalSlotKind::Transient
                } else {
                    LocalSlotKind::Client
                };
                if !claim_local_slot(local.as_deref_mut(), kind) {
                    return None;
                }
                return Box::pin(establish_local_first_frame(
                    msg,
                    conn.server,
                    conn.writer,
                    conn.attached_ids,
                ))
                .await;
            }
            Err(ScribeError::Io { .. }) => {
                debug!("client disconnected before Hello");
                return None;
            }
            Err(ScribeError::Deserialization { source }) => {
                // `read_message` consumed the complete length-prefixed payload
                // before decoding it, so only this error preserves alignment.
                // Keep waiting for a valid first message on the next frame.
                warn!("skipping undecodable client frame before Hello: {source}");
            }
            Err(e) => {
                // A framing/size failure can leave bytes from the declared frame
                // unread. Continuing could interpret payload bytes as a prefix.
                warn!("failed to read Hello frame; closing connection: {e}");
                return None;
            }
        }
    }
}

/// Move a classified local connection into its established admission pool,
/// logging a refusal. Remote connections pass `None` and are always admitted
/// here: their caps were charged by their own transport's accept path.
fn claim_local_slot(slot: Option<&mut LocalSlot>, kind: LocalSlotKind) -> bool {
    let Some(slot) = slot else {
        return true;
    };
    if slot.claim(kind) {
        return true;
    }
    match kind {
        LocalSlotKind::Client => {
            warn!("connection limit ({MAX_CONNECTIONS}) reached, rejecting client");
        }
        LocalSlotKind::Transient => {
            warn!("transient limit ({MAX_TRANSIENT_CONNECTIONS}) reached, dropping action");
        }
    }
    false
}

/// Whether a local non-`Hello` first frame is a one-shot transient action — one
/// that answers at most one frame, registers no window, and closes — and so
/// belongs to the [`MAX_TRANSIENT_CONNECTIONS`] pool rather than the client pool.
/// Exactly the arms [`establish_local_first_frame`] and
/// [`establish_local_lan_first_frame`] answer with `None`; everything else is a
/// legacy no-`Hello` claim that registers a window and must hold a client slot.
/// A transient arm added there but missed here is
/// merely charged to the client pool — the pre-017 behavior, never a leak.
fn is_transient_first_frame(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::ListWindows
            | ClientMessage::QuitAll
            | ClientMessage::DispatchAction { .. }
            | ClientMessage::AgentRequest(_)
            | ClientMessage::CheckForUpdates
            | ClientMessage::ListReleases
            | ClientMessage::TriggerUpdate
            | ClientMessage::HookEvent(_)
            | ClientMessage::ListRemotePeers
            | ClientMessage::GetRemoteEnv
            | ClientMessage::ListLanPeers
            | ClientMessage::GetLanEnv
            | ClientMessage::GetLanDialIdentity
            | ClientMessage::ListTrustedNetworks
            | ClientMessage::AddCurrentNetworkTrusted
            | ClientMessage::RemoveTrustedNetwork { .. }
            | ClientMessage::ListTrustedDevices
            | ClientMessage::RevokeTrustedDevice { .. }
    )
}

/// Read the next client frame under an optional read bound. Remote (TCP)
/// connections carry [`REMOTE_IDLE_READ_TIMEOUT`] on every read so an abandoned
/// peer's scarce slot is reclaimed; a local connection carries
/// [`LOCAL_PRE_HELLO_TIMEOUT`] on its first frame only and reads untimed
/// (`None`) afterward, since an idle window is legitimate. Returns `None` when
/// the bound expires — the caller treats that as a disconnect.
async fn read_client_frame<R>(
    reader: &mut R,
    idle_timeout: Option<std::time::Duration>,
) -> Option<Result<ClientMessage, ScribeError>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let read = read_message::<ClientMessage, _>(reader);
    let Some(limit) = idle_timeout else {
        return Some(read.await);
    };
    tokio::time::timeout(limit, read).await.ok()
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
        ClientMessage::QuitAll => {
            let quit = ServerMessage::QuitRequested;
            for window_writer in connected_local_window_writers(&server.window_shares).await {
                send_message(&window_writer, &quit).await;
            }
            None
        }
        ClientMessage::ListWindows => {
            handle_list_windows(&server.window_shares, &server.workspace_manager, writer).await;
            None
        }
        ClientMessage::DispatchAction { window_id, action } => {
            handle_transient_dispatch_action(window_id, action, &server.window_shares, writer)
                .await;
            None
        }
        ClientMessage::AgentRequest(request) => {
            handle_transient_agent_request(server, writer, &request).await;
            None
        }
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
        // Feature 014 LAN local-only transient first frames (and the legacy
        // no-`Hello` claim) are dispatched in a sibling to keep each function under
        // the cognitive-complexity budget; they remain local-socket only by the
        // same `establish_client_window` construction.
        other => establish_local_lan_first_frame(other, server, writer, attached_ids).await,
    }
}

/// Feature 014 LAN-surface local-only transient first frames — the LAN analog of
/// the update/tailnet arms in [`establish_local_first_frame`], split into a sibling
/// so each stays under the cognitive-complexity budget. Every arm here is
/// local-socket only by the SAME construction as [`establish_local_first_frame`]:
/// the remote path refuses a non-`Hello` first frame in [`establish_client_window`]
/// and never reaches this dispatch, so no remote peer can enumerate a third
/// machine's LAN view or read/exfiltrate its device identity. A frame that is none
/// of these falls through to [`handle_legacy_client`] (a legacy no-`Hello` claim).
async fn establish_local_lan_first_frame(
    msg: ClientMessage,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> Option<WindowId> {
    match msg {
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
        ClientMessage::GetLanDialIdentity => {
            // Feature 014 (LAN dial-identity fix) transient local-only action: hand
            // this machine's OWN device identity (cert + sealed key) to a co-located
            // connecting `scribe-client` so the dialer never reads the OS keyring
            // from a different binary — on macOS the legacy SecKeychain per-item ACL
            // trusts only the creating binary (scribe-server), so a cross-binary key
            // read is denied. The server stays the SOLE keychain accessor; reply
            // once, close. Local-socket only by the same construction as `GetLanEnv`
            // — a non-Hello first frame is refused over any remote transport in
            // `establish_client_window`, so a remote peer can never exfiltrate this
            // machine's private device key.
            handle_transient_get_lan_dial_identity(writer).await;
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

/// Feature 014 (LAN dial-identity fix): serve a transient `GetLanDialIdentity` —
/// hand this machine's OWN device identity (public certificate DER + sealed
/// `PKCS#8` private-key DER) to a co-located connecting `scribe-client` so the
/// dialer can build its mutual-TLS identity WITHOUT reading the OS keyring from a
/// different binary. The server is the SOLE keychain accessor: on macOS the sealed
/// device key's legacy `SecKeychain` per-item ACL trusts only the creating binary,
/// so a cross-binary read is denied (errSecInteractionNotAllowed) with no usable
/// prompt. Unlike the read-only `GetLanEnv`, this MINTS the identity on first use
/// (`load_or_generate`), matching the owning side, so a first-ever dial from a
/// fresh install still presents a stable key. Fails closed on any keyring /
/// state-dir error to `available = false` with empty DER so the client aborts the
/// dial. The reply carries PRIVATE key material and is local-socket only by the
/// same construction as `GetLanEnv` — a non-`Hello` first frame is refused over any
/// remote transport in `establish_client_window`, so it never crosses a remote
/// link. Replies once and closes.
async fn handle_transient_get_lan_dial_identity(writer: &SharedWriter) {
    info!("transient client requested LAN dial identity");
    let reply = match crate::lan::identity::load_or_generate().await {
        Ok(identity) => ServerMessage::LanDialIdentity {
            available: true,
            cert_der: identity.cert_der().as_ref().to_vec(),
            private_key_pkcs8_der: identity.private_key_pkcs8_der(),
        },
        Err(error) => {
            debug!(%error, "LAN dial identity unavailable; failing closed");
            ServerMessage::LanDialIdentity {
                available: false,
                cert_der: Vec::new(),
                private_key_pkcs8_der: Vec::new(),
            }
        }
    };
    send_message(writer, &reply).await;
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
    /// Explicit claim action, if any.
    intent: ClaimIntent,
    /// Spec 020: the image subset this connection may actually render, already
    /// intersected with Scribe's v1 support and the master switch.
    terminal_images: TerminalImageCapabilities,
    /// CI run-bar protocol support advertised by this connection.
    ci_run_bar: bool,
    /// Agent control-surface protocol support advertised by this connection.
    agent_api: AgentApiCapability,
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
        &server.window_shares,
        &claim,
        &all_windows,
        writer,
        server.remote_control.sharing_snapshot(),
    )
    .await;

    match outcome {
        ClaimOutcome::Owned { window_id, other_windows, displaced } => {
            // Feature 013/015: a takeover swapped out the live controller(s) — notify
            // each so it freezes its last frame and offers reclaim (FR-007/FR-003).
            // In a shared mode EVERY attached participant is displaced (T010). This
            // is the only takeover side effect left outside the claim lock: the
            // capability bit and controller identity were already re-bound to this
            // claimant atomically under the lock (see `resolve_and_register_claim`),
            // so no stale clipboard-gating or policy state can survive the swap
            // (FR-014), and the transition itself is traced there. The
            // clipboard-bridge routing then follows automatically — the new
            // controller's AttachSessions re-points each session's client writer,
            // and the displaced clients' later disconnect can no longer detach them
            // (see `detach_sessions`' ptr-eq guard).
            for displaced_writer in &displaced {
                send_message(displaced_writer, &claim.controller.window_taken_over()).await;
            }

            if !other_windows.is_empty() {
                info!(%window_id, other_count = other_windows.len(), "Welcome includes other_windows — client will spawn additional processes");
            }
            // Feature 015 self-id: tell the client its own registered participant id
            // so it can match itself in a `ShareRoster` exactly (its own `is_holder`).
            let (participant_id, beads_detail, registered_agent_api) = {
                let shares = server.window_shares.read().await;
                let share = shares.get(&window_id);
                (
                    share.and_then(|share| share.participant_id_for_writer(writer)),
                    beads_detail_connection_available(
                        share,
                        writer,
                        matches!(claim.controller, ControllerIdentity::Remote { .. }),
                    ),
                    share.and_then(|share| share.participant_for_writer(writer)).is_some_and(
                        |participant| participant.agent_api == AgentApiCapability::Supported,
                    ),
                )
            };
            let beads_write = beads_detail && BeadsBoardCache::write_available();
            let welcome = ServerMessage::Welcome {
                window_id,
                other_windows,
                clipboard_gating: true,
                participant_id,
                terminal_images: claim.terminal_images,
                beads_detail,
                beads_write,
                // Flow has the identical local-owner/unshared admission as
                // detail reads, so neither capability widens the trust boundary.
                beads_flow: beads_detail,
                pi_provider: true,
                agent_api: claim.agent_api == AgentApiCapability::Supported,
            };
            send_message(writer, &welcome).await;

            info!(
                %window_id,
                client_clipboard_gating = claim.clipboard_gating,
                client_agent_api = registered_agent_api,
                "client identified via Hello"
            );

            announce_share_join(server, window_id, claim.controller, !displaced.is_empty()).await;
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
                // A lost-control landing registers no participant.
                participant_id: None,
                terminal_images: claim.terminal_images,
                beads_detail: false,
                beads_write: false,
                beads_flow: false,
                pi_provider: true,
                agent_api: claim.agent_api == AgentApiCapability::Supported,
            };
            send_message(writer, &welcome).await;
            send_message(writer, &current.window_taken_over()).await;
            info!(%window_id, "remote reconnect landed on a controlled window; sent lost-control");
            window_id
        }
    }
}

/// Feature 015 (T022/T023): after a `Hello` registers, announce the roster to every
/// participant (a no-op in `SingleController`) and audit a remote participant's
/// share join — an additive join or a remote adopting an owner-less window, but NOT
/// a takeover, whose membership change is already covered by the 013 accept +
/// control-transition audit.
async fn announce_share_join(
    server: &IpcServerState,
    window_id: WindowId,
    controller: &ControllerIdentity,
    was_takeover: bool,
) {
    broadcast_share_roster(server, window_id).await;
    if !was_takeover && matches!(controller, ControllerIdentity::Remote { .. }) {
        audit_membership_event(window_id, "join", controller);
    }
}

async fn handle_legacy_client(
    msg: ClientMessage,
    server: &IpcServerState,
    writer: &SharedWriter,
    attached_ids: &AttachedSessionIds,
) -> WindowId {
    let window_id = WindowId::new();
    // A fresh `WindowId::new()` cannot collide, so a direct insert is safe here.
    // Any path whose window ID *can* collide must go through
    // `resolve_and_register_claim` so the check and the insert stay atomic. Legacy
    // clients are always local and advertise no clipboard gating, so the share
    // holds a single local participant with gating off — byte-identical to the
    // pre-015 `connected_clients` + `window_controllers` inserts.
    let participant = Participant::local(writer, false);
    server
        .window_shares
        .write()
        .await
        .insert(window_id, WindowShare::new_single_controller(participant));
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
        /// Writers displaced by a takeover, each notified `WindowTakenOver`: the
        /// sole previous owner in `SingleController`, or EVERY attached participant
        /// when the takeover ends an active shared share (feature 015 T010,
        /// FR-003). Empty for a fresh / adopted / first claim and for an additive
        /// join.
        displaced: Vec<SharedWriter>,
    },
    LostControl {
        window_id: WindowId,
        /// Identity of the controller that keeps the window, to name in the
        /// immediate lost-control `WindowTakenOver` (FR-011). Also names the
        /// current controller when an additive join is refused over the
        /// participant limit (feature 015 T011).
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
    /// Feature 015 (T010): a non-takeover claim of a still-connected window while
    /// sharing is enabled — register additively rather than lost-control.
    AdditiveJoin { window_id: WindowId },
}

/// How a claim should treat a target window that is already connected
/// (feature 013). Derived from `takeover` + the window share's sharing mode +
/// explicit local join intent + the connection's transport so
/// [`resolve_window_claim`] takes one mode instead of several bools.
#[derive(Clone, Copy)]
enum ClaimMode {
    /// Explicit takeover (picker attach or banner reclaim) — swap the writer.
    Takeover,
    /// Feature 015 (T010): a non-takeover claim naming a connected window while
    /// sharing is enabled — join that window's share additively instead of
    /// lost-control (remote) or a fresh window (local). The local half is what
    /// lets a second process on the OWNING machine — a second client, or the
    /// visual E2E rig's `scribe-test` daemon and GPUI client — view and type into
    /// one window's panes at once, which is the whole point of a shared mode; the
    /// remote half is feature 015's original tailnet join.
    ShareJoin,
    /// Remote auto-reconnect (sharing off) — lost-control rather than a silent
    /// seize.
    RemoteReconnect,
    /// Local plain claim, sharing off — assign a different/new window (today's
    /// behavior).
    LocalPlain,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimIntent {
    Plain,
    Takeover,
    Join,
}

/// Pick the [`ClaimMode`] a `Hello` claim resolves under (feature 015 T010,
/// extended by 016 for the local path).
///
/// An explicit takeover always wins. With sharing OFF the two legacy arms stand:
/// a remote reconnect is lost-control, a local plain claim gets a different/new
/// window. With sharing ON, remote claims keep their additive behavior. Local
/// claims join only when `Hello.join_window` explicitly identifies the launch
/// as a share join; a named restore claim stays `LocalPlain`.
///
/// Stock local clients may name a restored window, so `window_id.is_some()` is
/// not proof of join intent.
fn claim_mode_for(
    intent: ClaimIntent,
    controller: &ControllerIdentity,
    sharing_mode: scribe_config::SharingMode,
) -> ClaimMode {
    if intent == ClaimIntent::Takeover {
        return ClaimMode::Takeover;
    }
    let sharing_enabled = !matches!(sharing_mode, scribe_config::SharingMode::SingleController);
    if matches!(controller, ControllerIdentity::Remote { .. }) {
        return if sharing_enabled { ClaimMode::ShareJoin } else { ClaimMode::RemoteReconnect };
    }
    if sharing_enabled && intent == ClaimIntent::Join {
        ClaimMode::ShareJoin
    } else {
        ClaimMode::LocalPlain
    }
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
                let other_windows = windows_in_stable_order(windows_with_sessions)
                    .into_iter()
                    .filter(|wid| *wid != window_id && !connected.contains_key(wid))
                    .collect();
                return ClaimResolution::Takeover { window_id, other_windows };
            }
            ClaimMode::ShareJoin => {
                return ClaimResolution::AdditiveJoin { window_id };
            }
            ClaimMode::RemoteReconnect => return ClaimResolution::LostControl { window_id },
            // Local, no takeover, sharing off: fall through to the unchanged
            // assignment path, which assigns a different/new window (never
            // displaces).
            ClaimMode::LocalPlain => {}
        }
    }
    let (assigned, other_windows) =
        resolve_window_assignment(hello_window_id, windows_with_sessions, connected);
    ClaimResolution::Assign { assigned, other_windows }
}

/// Atomically resolve a window claim and register the connecting client's
/// participant — writer, controller identity, AND clipboard-gating bit — under one
/// `window_shares` write-lock hold (feature 015 T006, D1).
///
/// Holding the write lock across the check and the insert makes the claim
/// indivisible: concurrent `Hello`s for the same window can never both register
/// (the pre-013 TOCTOU race that a post-update reconnect burst triggers), and a
/// near-simultaneous takeover burst resolves deterministically to exactly one
/// controller. Folding the three retired per-window maps into one `WindowShare`
/// entry means the controller identity and the spec-010 capability bit now travel
/// on the registered `Participant` — no separate map can drift from the writer,
/// and the tri-lock ordering hazard (a losing takeover overwriting the winner's
/// gating bit) is gone by construction (FR-014).
///
/// - `Assign`: insert a fresh single-controller share for the resolved window.
/// - `Takeover`: replace the connected window's share with the new participant and
///   return the displaced controller's writer so the caller can send it
///   `WindowTakenOver`.
/// - `LostControl`: leave the current owner's share untouched (this connection is
///   NOT registered) and report the current controller's identity to name.
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
    window_shares: &WindowShares,
    claim: &HelloClaim<'_>,
    all_windows: &HashSet<WindowId>,
    writer: &SharedWriter,
    sharing: SharingSnapshot,
) -> ClaimOutcome {
    let mode = claim_mode_for(claim.intent, claim.controller, sharing.mode);
    // Resolve and register the participant as one indivisible transition under the
    // single share lock, capturing the displaced controller (if any) for the
    // post-lock trace. The trace is emitted after the guard drops so a slow
    // blocking log writer never stalls a concurrent claim/detach/list.
    let (outcome, displaced_controller) = {
        let mut shares = window_shares.write().await;
        match resolve_window_claim(claim.requested_window_id, mode, all_windows, &shares) {
            ClaimResolution::Assign { assigned, other_windows } => {
                // A fresh share adopts the current mode so the owner's window (and a
                // remote adopting an owner-less window) reflects the live setting.
                let participant = Participant::from_claim(claim, writer);
                let share = WindowShare::new(
                    participant,
                    sharing.mode,
                    sharing.control_acquisition,
                    sharing.participant_limit,
                );
                shares.insert(assigned, share);
                (
                    ClaimOutcome::Owned {
                        window_id: assigned,
                        other_windows,
                        displaced: Vec::new(),
                    },
                    None,
                )
            }
            ClaimResolution::Takeover { window_id, other_windows } => {
                // Feature 015 (T010, FR-003): an exclusive takeover ends any active
                // share for EVERY attached participant — each is displaced and
                // notified `WindowTakenOver` — and the claimer becomes the sole
                // `SingleController` owner.
                let participant = Participant::from_claim(claim, writer);
                let previous =
                    shares.insert(window_id, WindowShare::new_single_controller(participant));
                let displaced = previous.as_ref().map(WindowShare::all_writers).unwrap_or_default();
                let displaced_controller = previous.and_then(|s| s.controller_identity().cloned());
                (ClaimOutcome::Owned { window_id, other_windows, displaced }, displaced_controller)
            }
            ClaimResolution::AdditiveJoin { window_id } => {
                register_additive_join(&mut shares, window_id, claim, writer)
            }
            ClaimResolution::LostControl { window_id } => {
                let current = shares
                    .get(&window_id)
                    .and_then(|s| s.controller_identity().cloned())
                    .unwrap_or(ControllerIdentity::Local);
                (ClaimOutcome::LostControl { window_id, controller: current }, None)
            }
        }
    };

    log_control_transition(&outcome, claim.controller, displaced_controller.as_ref());
    outcome
}

/// Register an additive participant in a connected window's share (feature 015
/// T010/T011), or refuse over the participant limit. Runs under the caller's
/// `window_shares` write lock. Enforces `participant_limit` on REMOTE participants
/// only (the local owner is exempt, FR-007/FR-018): a full share is left
/// undisturbed and the joiner gets the current controller's lost-control notice.
fn register_additive_join(
    shares: &mut HashMap<WindowId, WindowShare>,
    window_id: WindowId,
    claim: &HelloClaim<'_>,
    writer: &SharedWriter,
) -> (ClaimOutcome, Option<ControllerIdentity>) {
    let Some(share) = shares.get_mut(&window_id) else {
        // Window vanished between resolve and register; treat as a fresh claim.
        let participant = Participant::from_claim(claim, writer);
        shares.insert(window_id, WindowShare::new_single_controller(participant));
        return (
            ClaimOutcome::Owned { window_id, other_windows: Vec::new(), displaced: Vec::new() },
            None,
        );
    };
    if let Some(limit) = share.participant_limit
        && share.remote_participant_count() >= limit as usize
    {
        let current = share.controller_identity().cloned().unwrap_or(ControllerIdentity::Local);
        info!(%window_id, limit, "additive join refused: participant limit reached");
        return (ClaimOutcome::LostControl { window_id, controller: current }, None);
    }
    share.add_participant(Participant::from_claim(claim, writer));
    let locality = match claim.controller {
        ControllerIdentity::Local => "local",
        ControllerIdentity::Remote { .. } => "remote",
    };
    info!(%window_id, locality, "participant joined share additively");
    (ClaimOutcome::Owned { window_id, other_windows: Vec::new(), displaced: Vec::new() }, None)
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
    displaced_id: Option<&ControllerIdentity>,
) {
    match outcome {
        // A takeover is the only `Owned` outcome that displaces a live writer.
        ClaimOutcome::Owned { window_id, displaced, .. } if !displaced.is_empty() => {
            info!(
                %window_id,
                new_controller = %new_controller.transition_label(),
                displaced_controller =
                    %displaced_id.map_or(Cow::Borrowed("unknown"), ControllerIdentity::transition_label),
                displaced_count = displaced.len(),
                "control transition: window taken over"
            );
        }
        ClaimOutcome::Owned { window_id, .. } => {
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
    window_shares: &WindowShares,
    requested_window_id: Option<WindowId>,
    all_windows: &HashSet<WindowId>,
    writer: &SharedWriter,
) -> (WindowId, Vec<WindowId>) {
    let local = ControllerIdentity::Local;
    let claim = HelloClaim {
        requested_window_id,
        clipboard_gating: false,
        intent: ClaimIntent::Plain,
        controller: &local,
        terminal_images: TerminalImageCapabilities::default(),
        ci_run_bar: false,
        agent_api: AgentApiCapability::Unsupported,
    };
    match resolve_and_register_claim(
        window_shares,
        &claim,
        all_windows,
        writer,
        SharingSnapshot::default(),
    )
    .await
    {
        ClaimOutcome::Owned { window_id, other_windows, .. } => (window_id, other_windows),
        ClaimOutcome::LostControl { window_id, .. } => (window_id, Vec::new()),
    }
}

/// Release a window's share only if `writer` is still its OWNER connection. The
/// `SharedWriter` Arc is the connection's identity token: a stale disconnect from a
/// client already superseded by a newer owner must not evict it — doing so makes
/// the window look unconnected and triggers a duplicate respawn. In `SingleController`
/// the owner is the `LegacyExclusive` writer (byte-identical to feature 013); in a
/// shared mode it is the local owner (a remote holder/viewer leaving is handled by
/// `remove_participant_by_writer`, not here). Returns whether the registry is now
/// empty, for settings-shutdown scheduling.
fn release_window_if_owned(
    shares: &mut HashMap<WindowId, WindowShare>,
    window_id: WindowId,
    writer: &SharedWriter,
) -> bool {
    if shares.get(&window_id).is_some_and(|share| share.is_owner_connection(writer)) {
        shares.remove(&window_id);
    }
    shares.is_empty()
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
/// Feature 015 (T012): whether `writer` may attach to (view) a window's sessions.
/// In `SingleController` mode only the registered controller may — byte-identical
/// to feature 013's `AttachSessions` authorization. In a shared mode ANY attached
/// participant may attach and view (additive), so a viewer receives the live
/// stream without owning the window. Returns the share mode so the caller knows
/// whether to attach the sink additively (shared) or replace it (legacy).
async fn connection_may_attach(
    window_shares: &WindowShares,
    window_id: WindowId,
    writer: &SharedWriter,
) -> Option<scribe_config::SharingMode> {
    let shares = window_shares.read().await;
    let share = shares.get(&window_id)?;
    let allowed = match share.mode {
        scribe_config::SharingMode::SingleController => share.is_controlled_by(writer),
        scribe_config::SharingMode::SharedSingleTypist | scribe_config::SharingMode::FreeForAll => {
            share.participant_for_writer(writer).is_some()
        }
    };
    allowed.then_some(share.mode)
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
        ClientMessage::CreateSession { .. }
            | ClientMessage::KeyInput { .. }
            | ClientMessage::Resize { .. }
            | ClientMessage::CloseSession { .. }
            | ClientMessage::CloseWindow { .. }
            | ClientMessage::FocusChanged { .. }
            | ClientMessage::SearchRequest { .. }
            | ClientMessage::SearchClosed { .. }
    )
}

/// Feature 015 (T008, D2): the mode-aware replacement for the feature-013
/// `Arc::ptr_eq` single-writer guard. Whether `writer`'s connection is authorized
/// to apply the gated `msg` against `window_id`, given the window share's mode. A
/// `false` result drops the message safely (FR-006), exactly as a post-takeover
/// barred connection is dropped today.
///
/// - `SingleController` → the legacy `Arc::ptr_eq` check against the sole holder,
///   including `Resize` (the legacy controller-gated grid-set); byte-identical to
///   feature 013.
/// - `SharedSingleTypist` → gated actions follow the `SingleTypist` holder;
///   `Resize` is exempt (an ungated per-participant viewport report accepted from
///   any attached participant, D3, consumed by T014).
/// - `FreeForAll` → interim: `KeyInput` and `Resize` are admitted from any
///   attached participant; the lifecycle / focus / search actions have no control
///   holder to follow and fall to the owning machine (the always-present
///   `ControllerIdentity::Local` participant), per spec Assumptions (finer split
///   lands with T029).
///
/// With every foundational share built in `SingleController` mode, only the first
/// arm executes today; the shared-mode arms are wired for US1/US4.
async fn connection_may_type(
    window_shares: &WindowShares,
    window_id: WindowId,
    writer: &SharedWriter,
    msg: &ClientMessage,
) -> bool {
    let shares = window_shares.read().await;
    let Some(share) = shares.get(&window_id) else {
        return false;
    };
    match share.mode {
        scribe_config::SharingMode::SingleController => share.is_controlled_by(writer),
        scribe_config::SharingMode::SharedSingleTypist => {
            // `Resize` is an ungated viewport report from any attached participant.
            if matches!(msg, ClientMessage::Resize { .. }) {
                return share.participant_for_writer(writer).is_some();
            }
            match &share.control {
                ControlState::SingleTypist { holder: Some(holder), .. } => {
                    share.participants.get(holder).is_some_and(|p| Arc::ptr_eq(&p.writer, writer))
                }
                _ => false,
            }
        }
        scribe_config::SharingMode::FreeForAll => {
            let Some(participant) = share.participant_for_writer(writer) else {
                return false;
            };
            match msg {
                ClientMessage::KeyInput { .. } | ClientMessage::Resize { .. } => true,
                // Lifecycle / focus / search: owner (`Local`) only in this mode.
                _ => matches!(participant.identity, ControllerIdentity::Local),
            }
        }
    }
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
    // Post-handshake reads carry the remote idle-read timeout so a vanished
    // peer's slot is reclaimed; local reads stay untimed (the pre-Hello bound
    // applies only to the first frame).
    let idle_timeout = is_remote.then_some(REMOTE_IDLE_READ_TIMEOUT);
    loop {
        // Race each client-message read against the sever signal so a disable
        // (T023) drops a remote connection out of its loop; on sever, fall through
        // to the caller's normal detach cleanup. Local connections pass no sever
        // channel, so the sever arm never fires.
        let read = tokio::select! {
            biased;
            () = await_sever(sever_rx.as_mut()) => return LoopExit::Severed,
            read = read_client_frame(reader, idle_timeout) => read,
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
            Some(Err(ScribeError::Deserialization { source })) => {
                // The frame body is already consumed, leaving the next length
                // prefix aligned. Discard only this malformed message.
                warn!(%window_id, "skipping undecodable client frame: {source}");
                continue;
            }
            Some(Err(e)) => {
                // I/O and size failures do not guarantee that the declared frame
                // was consumed, so the stream is unsafe to continue parsing.
                warn!(%window_id, "failed to read client frame; disconnecting: {e}");
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

/// The outcome of resolving one connection's departure from a window's share under
/// the `window_shares` write lock (feature 015). All sends happen after the lock
/// drops (D5).
struct WindowDetach {
    /// The released controller identity when this connection was the window's owner
    /// (share torn down); `None` for a non-owner departure. Also drives the
    /// Controlled → Unconnected trace.
    released_controller: Option<ControllerIdentity>,
    /// The departing participant's identity, for the membership audit.
    left_identity: Option<ControllerIdentity>,
    /// A non-owner (shared-mode viewer/holder) was removed from a surviving share.
    viewer_left: bool,
    /// Remote participants to notify `ShareEnded { OwnerClosed }` (owner tore down
    /// an active shared share, T032 deviation #2).
    owner_close_remotes: Vec<SharedWriter>,
    /// The share registry is now empty (settings-shutdown scheduling).
    registry_empty: bool,
}

/// Resolve a connection's departure from `window_id`'s share under the caller's
/// write lock (feature 015). The share is torn down only when its OWNER leaves (the
/// `LegacyExclusive` writer in `SingleController`, or the local owner in a shared
/// mode); a stale takeover-displaced client is not the owner, so the share is left
/// intact (FR-014); a remote holder/viewer leaving only removes itself (FR-016,
/// holder loss handled in `remove_participant_by_writer`).
fn resolve_window_detach(
    shares: &mut HashMap<WindowId, WindowShare>,
    window_id: WindowId,
    writer: &SharedWriter,
) -> WindowDetach {
    let left_identity = shares
        .get(&window_id)
        .and_then(|share| share.participant_for_writer(writer))
        .map(|p| p.identity.clone());
    let is_owner = shares.get(&window_id).is_some_and(|share| share.is_owner_connection(writer));
    if is_owner {
        let released_controller =
            shares.get(&window_id).and_then(|s| s.controller_identity().cloned());
        let owner_close_remotes = shares
            .get(&window_id)
            .filter(|s| !matches!(s.mode, scribe_config::SharingMode::SingleController))
            .map(|s| {
                s.participants
                    .values()
                    .filter(|p| matches!(p.transport, ParticipantTransport::Remote))
                    .map(|p| Arc::clone(&p.writer))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let empty = release_window_if_owned(shares, window_id, writer);
        WindowDetach {
            released_controller,
            left_identity,
            viewer_left: false,
            owner_close_remotes,
            registry_empty: empty,
        }
    } else {
        let viewer_left = shares
            .get_mut(&window_id)
            .and_then(|share| share.remove_participant_by_writer(writer))
            .is_some();
        WindowDetach {
            released_controller: None,
            left_identity,
            viewer_left,
            owner_close_remotes: Vec::new(),
            registry_empty: shares.is_empty(),
        }
    }
}

async fn detach_client_window(
    window_id: WindowId,
    server: &IpcServerState,
    attached_ids: &AttachedSessionIds,
    writer: &SharedWriter,
    severed: bool,
) {
    server.agent_api.fail_actions_for_client(action_client_key(writer));
    server.github_ci_tracker.drop_detail_writer(Arc::clone(writer));
    let attached_ids = attached_snapshot(attached_ids).await;
    clear_focused_issues_for_disconnect(
        &server.live_sessions,
        &server.window_shares,
        window_id,
        &attached_ids,
        writer,
    )
    .await;
    // Detach only the sessions still routed to THIS connection. A feature-013
    // takeover may already have re-pointed them at the new controller, whose
    // output + clipboard-bridge routing (T016) must survive this old client's
    // disconnect; the ptr-eq guard inside `detach_sessions` enforces that.
    detach_sessions(&server.live_sessions, &attached_ids, writer).await;

    // Determine ownership and release under one share-registry write lock (see
    // `resolve_window_detach`), then apply the sends/audit with no lock held (D5).
    let detach = {
        let mut shares = server.window_shares.write().await;
        resolve_window_detach(&mut shares, window_id, writer)
    };

    // Feature 013 (T027): trace the Controlled → Unconnected transition, naming the
    // controller whose window just became reattachable.
    if let Some(released) = &detach.released_controller {
        debug!(
            %window_id,
            controller = %released.transition_label(),
            "control transition: window released"
        );
    }
    // Feature 015 (T023/T033): audit the share membership departure (FR-015/SC-007)
    // whenever this connection was a participant — `eject` for a forced sever (device
    // revoke / disable), `leave` for a clean disconnect.
    if let Some(identity) = &detach.left_identity {
        audit_membership_event(window_id, if severed { "eject" } else { "leave" }, identity);
    }
    // Feature 015 (T032, deviation #2): notify each remote participant that the
    // owner ended the share.
    for remote in &detach.owner_close_remotes {
        send_message(
            remote,
            &ServerMessage::ShareEnded {
                window_id,
                reason: scribe_common::protocol::ShareEndReason::OwnerClosed,
            },
        )
        .await;
    }
    // Feature 015 (T019/T022/T014): a participant leaving a still-running share
    // regrows the min-of-viewports grid and re-broadcasts the roster (which reflects
    // any holder loss) to the remaining participants.
    if detach.viewer_left {
        apply_authoritative_grid(server, window_id).await;
        broadcast_share_roster(server, window_id).await;
    }
    let still_owned = detach.released_controller.is_some();
    let last_client_disconnected = detach.registry_empty;
    info!(%window_id, still_owned, "client connection closed; window released if still owned");
    if last_client_disconnected {
        schedule_settings_shutdown_if_no_clients(Arc::clone(&server.window_shares));
    }
}

/// Clear any liveness binding before its owning local controller disconnects.
///
/// `attached_ids` can contain sessions a displaced connection used to own, so
/// prove this writer is still the local single-controller owner before clearing.
/// A remote or shared connection fails that proof and sees no liveness frame.
async fn clear_focused_issues_for_disconnect(
    live_sessions: &LiveSessionRegistry,
    window_shares: &WindowShares,
    window_id: WindowId,
    attached_ids: &HashSet<SessionId>,
    writer: &SharedWriter,
) {
    let owns_flow = {
        let shares = window_shares.read().await;
        let Some(share) = shares.get(&window_id) else {
            return;
        };
        share.local_participant().is_some_and(|local| Arc::ptr_eq(&local.writer, writer))
            && beads_detail_connection_available(Some(share), writer, false)
    };
    if !owns_flow {
        return;
    }

    let session_ids = {
        let sessions = live_sessions.read().await;
        attached_ids
            .iter()
            .copied()
            .filter(|session_id| {
                sessions.get(session_id).is_some_and(|session| session.env_window_id == window_id)
            })
            .collect::<Vec<_>>()
    };
    for session_id in session_ids {
        set_focused_issue(session_id, None, live_sessions, window_shares).await;
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
            detach_one_session(session, *id, writer).await;
        }
    }
}

/// Detach a single session's sink for `writer` (feature 015 T007). Removes only
/// this connection's sink (`Arc::ptr_eq`): a session already re-pointed by a
/// takeover keeps the new controller's sink. When the set empties (the sole legacy
/// sink left), clear the session attachment — byte-identical to the pre-015
/// single-slot clear.
///
/// Emptying the set also drops whatever size the resize pacer is holding.
/// Nothing else cancels a timer armed mid-drag, and a session that comes back
/// through a fresh attach comes back at the new client's geometry — letting the
/// pre-detach report mature would overwrite that grid up to an interval later.
/// A detach that leaves other sinks attached keeps the pending size, since the
/// drag it belongs to is still someone's.
async fn detach_one_session(session: &LiveSession, id: SessionId, writer: &SharedWriter) {
    let now_empty = {
        let mut client_writer = lock_sinks(&session.client_writer);
        if !client_writer.detach(writer) {
            return;
        }
        client_writer.is_empty()
    };
    if now_empty {
        lock_resize_pacer(&session.resize_pacer).discard_pending();
        clear_session_attachment(&session.attachment).await;
    }
    info!(%id, "session detached (client disconnected)");
}

/// Close the singleton settings window once the client registry stays empty
/// long enough to rule out a hot-reload or reconnect race.
fn schedule_settings_shutdown_if_no_clients(window_shares: WindowShares) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if window_shares.read().await.is_empty() {
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
    // window's registered controller; local clients always are. `AttachSessions` is guarded in
    // `filter_attachable_sessions`.
    let shares = &context.server.window_shares;
    if requires_window_control(&msg)
        && !connection_may_type(shares, context.window_id, context.writer, &msg).await
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
        | ClientMessage::SearchRequest { .. }
        | ClientMessage::SearchClosed { .. }) => {
            dispatch_session_message(msg, context).await;
        }
        msg @ (ClientMessage::Subscribe { .. }
        | ClientMessage::RequestSnapshot { .. }
        | ClientMessage::CreateWorkspace
        | ClientMessage::CloseWorkspace { .. }
        | ClientMessage::MoveSession { .. }
        | ClientMessage::ListSessions
        | ClientMessage::RequestBeadsBoard { .. }
        | ClientMessage::RequestBeadsIssueDetail { .. }
        | ClientMessage::RequestBeadsEpicGraph { .. }
        | ClientMessage::BeadsIssueWrite { .. }
        | ClientMessage::ReportWorkspaceTree { .. }) => {
            dispatch_workspace_message(msg, context).await;
        }
        msg @ (ClientMessage::CloseWindow { .. }
        | ClientMessage::QuitAll
        | ClientMessage::TriggerUpdate
        | ClientMessage::DismissUpdate
        | ClientMessage::DismissCiRun { .. }
        | ClientMessage::SetCiRunDetailsInterest { .. }
        | ClientMessage::CheckForUpdates
        | ClientMessage::ListReleases
        | ClientMessage::ListWindows
        | ClientMessage::DispatchAction { .. }
        | ClientMessage::ActionCompleted { .. }
        | ClientMessage::AgentPromptResponse { .. }
        | ClientMessage::EnvPreflight) => {
            dispatch_window_message(msg, context).await;
        }
        msg @ (ClientMessage::ClipboardPromptResponse { .. }
        | ClientMessage::ClipboardBridgeReadReply { .. }) => {
            dispatch_clipboard_answer(msg, context).await;
        }
        ClientMessage::LanApprovalDecision { request_id, approve } => {
            handle_lan_approval_decision(context, request_id, approve);
        }
        // The connect picker enumerates peers from within its already-`Hello`ed
        // session connection (feature 013/014), so the local-only peer-list
        // queries are answered here too — not only on the pre-`Hello` transient
        // path. Gated to LOCAL connections: a remote (tailnet/LAN) peer must never
        // enumerate this machine's tailnet/LAN view, so a remote sender falls
        // through to the ignore arm (the same guarantee the pre-`Hello` path gets
        // from `establish_client_window` refusing non-`Hello` frames).
        msg @ (ClientMessage::ListRemotePeers | ClientMessage::ListLanPeers)
            if !context.is_remote =>
        {
            dispatch_local_query_message(msg, context).await;
        }
        // Feature 015 (T017/T018): v3 control-transfer messages. Not in
        // `requires_window_control`, so a viewer can send them even while its input
        // is gated; the handler authorizes per participant/mode under the share lock.
        msg @ (ClientMessage::ControlClaim { .. }
        | ClientMessage::ControlRequest { .. }
        | ClientMessage::ControlGrant { .. }) => {
            handle_control_message(msg, context).await;
        }
        other => debug!(?other, "unhandled client message"),
    }
}

async fn dispatch_clipboard_answer(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
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
        other => debug!(?other, "ignored non-clipboard answer"),
    }
}

/// Answer the local-only connect-picker peer-list queries — `ListRemotePeers`
/// (013 tailnet) and `ListLanPeers` (014 LAN) — that arrive on the connecting
/// client's live post-`Hello` session connection. Split out of
/// [`dispatch_message`] to keep its match under Clippy's cognitive-complexity
/// budget; the caller has already gated this to local connections, so both
/// replies stay local-socket only.
async fn dispatch_local_query_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::ListRemotePeers => handle_transient_list_remote_peers(context.writer).await,
        ClientMessage::ListLanPeers => {
            handle_transient_list_lan_peers(context.server, context.writer).await;
        }
        other => debug!(?other, "ignored non-query message in local-query dispatcher"),
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
            ai_launch,
            shell_tool,
            env_envelope_id,
        } => {
            handle_create_session(
                CreateSessionRequest {
                    workspace_id,
                    split_direction,
                    cwd,
                    size,
                    command,
                    ai_launch,
                    shell_tool,
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
                &context.server.window_shares,
                context.attached_ids,
            )
            .await;
            context.server.workspace_manager.write().await.remove_session_from_window(session_id);
        }
        ClientMessage::Resize { session_id, size } => {
            handle_resize_message(session_id, size, context).await;
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
        ClientMessage::SearchClosed { session_id } => {
            handle_search_closed(session_id, context).await;
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
        ClientMessage::MoveSession { session_id, target_workspace } => {
            let moved = context
                .server
                .workspace_manager
                .write()
                .await
                .move_session(session_id, target_workspace);
            // The live-session record is what `SessionList` and handoff
            // serialize, so it must follow the membership move.
            if moved
                && let Some(live) = context.server.live_sessions.write().await.get_mut(&session_id)
            {
                live.workspace_id = target_workspace;
            }
        }
        ClientMessage::CloseWorkspace { workspace_id } => {
            context.server.workspace_manager.write().await.close_workspace(workspace_id);
        }
        ClientMessage::ListSessions => {
            handle_list_sessions(
                &context.server.live_sessions,
                &context.server.workspace_manager,
                context.writer,
                context.window_id,
                context.is_remote,
            )
            .await;
        }
        ClientMessage::RequestBeadsBoard { workspace_id, protocol_version } => {
            handle_request_beads_board(workspace_id, protocol_version, context).await;
        }
        ClientMessage::RequestBeadsIssueDetail { workspace_id, issue_id } => {
            handle_request_beads_issue_detail(workspace_id, issue_id, context).await;
        }
        ClientMessage::RequestBeadsEpicGraph { workspace_id, epic_id } => {
            handle_request_beads_epic_graph(workspace_id, epic_id, context).await;
        }
        ClientMessage::BeadsIssueWrite { workspace_id, issue_id, verb, guards } => {
            handle_beads_issue_write(workspace_id, issue_id, verb, guards, context).await;
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

// @lat: [[client#Client#Beads Board CLI Data Source]]
async fn handle_request_beads_board(
    workspace_id: WorkspaceId,
    protocol_version: u32,
    context: &ClientDispatchContext<'_>,
) {
    if protocol_version != BEADS_BOARD_PROTOCOL_VERSION {
        send_message(
            context.writer,
            &ServerMessage::BeadsBoard {
                workspace_id,
                protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
                state: BeadsBoardState::Unavailable {
                    message: "Scribe Beads-board protocol version mismatch".into(),
                },
            },
        )
        .await;
        return;
    }

    let project_root = {
        let workspaces = context.server.workspace_manager.read().await;
        workspaces
            .window_contains_workspace(context.window_id, workspace_id)
            .then(|| workspaces.workspace_info(workspace_id))
            .flatten()
            .and_then(|(_, _, _, root)| root)
    };
    let Some(project_root) = project_root else {
        send_message(
            context.writer,
            &ServerMessage::BeadsBoard {
                workspace_id,
                protocol_version,
                state: BeadsBoardState::NotDetected,
            },
        )
        .await;
        return;
    };

    let lookup = context.server.beads_boards.lookup(&project_root).await;
    send_message(
        context.writer,
        &ServerMessage::BeadsBoard { workspace_id, protocol_version, state: lookup.state },
    )
    .await;

    if lookup.refresh {
        let cache = context.server.beads_boards.clone();
        let writer = Arc::clone(context.writer);
        tokio::spawn(async move {
            let state = Box::pin(cache.refresh(lookup.key)).await;
            send_message(
                &writer,
                &ServerMessage::BeadsBoard { workspace_id, protocol_version, state },
            )
            .await;
        });
    }
}

async fn handle_request_beads_issue_detail(
    workspace_id: WorkspaceId,
    issue_id: String,
    context: &ClientDispatchContext<'_>,
) {
    let Some(project_root) = beads_detail_request_root(
        &context.server.workspace_manager,
        &context.server.window_shares,
        BeadsDetailRequest {
            window_id: context.window_id,
            writer: context.writer,
            is_remote: context.is_remote,
            workspace_id,
        },
    )
    .await
    else {
        debug!(
            window_id = %context.window_id,
            %workspace_id,
            "ignoring unauthorized Beads issue-detail request"
        );
        return;
    };

    match load_issue_detail(&project_root, &issue_id).await {
        Ok(DetailLoadResult::Found(detail)) => {
            send_message(
                context.writer,
                &ServerMessage::BeadsIssueDetail { workspace_id, issue_id, detail: Some(detail) },
            )
            .await;
        }
        Ok(DetailLoadResult::NotFound) => {
            send_message(
                context.writer,
                &ServerMessage::BeadsIssueDetail { workspace_id, issue_id, detail: None },
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(
                root = %project_root.display(),
                %issue_id,
                %error,
                "Beads issue-detail query failed"
            );
            send_error(context.writer, &error).await;
        }
    }
}

async fn handle_request_beads_epic_graph(
    workspace_id: WorkspaceId,
    epic_id: String,
    context: &ClientDispatchContext<'_>,
) {
    let Some(project_root) = beads_detail_request_root(
        &context.server.workspace_manager,
        &context.server.window_shares,
        BeadsDetailRequest {
            window_id: context.window_id,
            writer: context.writer,
            is_remote: context.is_remote,
            workspace_id,
        },
    )
    .await
    else {
        debug!(
            window_id = %context.window_id,
            %workspace_id,
            "ignoring unauthorized Beads epic-graph request"
        );
        return;
    };

    let outcome = context.server.beads_boards.epic_graph(&project_root, &epic_id).await;
    match &outcome {
        BeadsEpicGraphOutcome::NoGraph { reason } => {
            info!(
                root = %project_root.display(),
                %epic_id,
                ?reason,
                "Beads Flow graph refused"
            );
        }
        BeadsEpicGraphOutcome::Unavailable { message } => {
            warn!(
                root = %project_root.display(),
                %epic_id,
                %message,
                "Beads Flow graph unavailable"
            );
        }
        BeadsEpicGraphOutcome::Graph(_) => {}
    }
    send_message(context.writer, &ServerMessage::BeadsEpicGraph { workspace_id, epic_id, outcome })
        .await;
}

async fn handle_beads_issue_write(
    workspace_id: WorkspaceId,
    issue_id: String,
    verb: BeadsIssueWrite,
    guards: BeadsIssueWriteGuards,
    context: &ClientDispatchContext<'_>,
) {
    let Some(project_root) = beads_detail_request_root(
        &context.server.workspace_manager,
        &context.server.window_shares,
        BeadsDetailRequest {
            window_id: context.window_id,
            writer: context.writer,
            is_remote: context.is_remote,
            workspace_id,
        },
    )
    .await
    else {
        debug!(
            window_id = %context.window_id,
            %workspace_id,
            "ignoring unauthorized Beads issue-write request"
        );
        return;
    };

    let outcome = if BeadsBoardCache::write_available() {
        context.server.beads_boards.write_issue(&project_root, &issue_id, &verb, &guards).await
    } else {
        crate::beads_board::BeadsIssueWriteOutcome {
            result: BeadsIssueWriteResult::Failed {
                reason: "bd is not installed or executable".into(),
            },
            lock: None,
        }
    };
    let result = outcome.result.clone();
    send_message(
        context.writer,
        &ServerMessage::BeadsIssueWriteResult {
            workspace_id,
            issue_id: issue_id.clone(),
            result: result.clone(),
        },
    )
    .await;

    let BeadsIssueWriteResult::Applied { generation } = result else {
        return;
    };
    let Some(lock) = outcome.lock else {
        return;
    };
    let key = project_root.canonicalize().unwrap_or_else(|_| project_root.clone());
    let cache = context.server.beads_boards.clone();
    let workspace_manager = Arc::clone(&context.server.workspace_manager);
    let window_shares = Arc::clone(&context.server.window_shares);
    tokio::spawn(async move {
        let state = cache.refresh_after_write(key, generation, lock).await;
        push_beads_board_for_root(&workspace_manager, &window_shares, &project_root, state).await;
    });
}

async fn push_beads_board_for_root(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    project_root: &Path,
    state: BeadsBoardState,
) {
    let placements =
        workspace_manager.read().await.window_workspaces_for_project_root(project_root);
    let recipients = {
        let shares = window_shares.read().await;
        placements
            .into_iter()
            .filter_map(|(window_id, workspace_id)| {
                let share = shares.get(&window_id)?;
                let local = share.local_participant()?;
                beads_detail_connection_available(Some(share), &local.writer, false)
                    .then(|| (workspace_id, Arc::clone(&local.writer)))
            })
            .collect::<Vec<_>>()
    };
    for (workspace_id, writer) in recipients {
        send_message(
            &writer,
            &ServerMessage::BeadsBoard {
                workspace_id,
                protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
                state: state.clone(),
            },
        )
        .await;
    }
}

fn beads_detail_connection_available(
    share: Option<&WindowShare>,
    writer: &SharedWriter,
    is_remote: bool,
) -> bool {
    !is_remote
        && share.is_some_and(|share| {
            matches!(share.mode, scribe_config::SharingMode::SingleController)
                && share.is_owner_connection(writer)
        })
}

/// Store one live agent's exact Beads issue binding and publish it only to the
/// local single-controller owner of that session's window.
///
/// Hook ingress calls this seam for its future `issue_focused` event. A missing
/// session is deliberately a no-op: helper processes can outlive their PTY.
/// The local-owner gate matches Flow graph admission, so a remote or shared
/// participant never learns which issue an agent is working on.
pub async fn set_focused_issue(
    session_id: SessionId,
    issue_id: Option<String>,
    live_sessions: &LiveSessionRegistry,
    window_shares: &WindowShares,
) {
    let window_id = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return;
        };
        if session.focused_issue == issue_id {
            return;
        }
        session.focused_issue.clone_from(&issue_id);
        session.env_window_id
    };

    let recipient = focused_issue_recipient(window_id, window_shares).await;
    if let Some(writer) = recipient {
        send_message(&writer, &ServerMessage::IssueFocused { session_id, issue_id }).await;
    }
}

/// Return the one connection allowed to receive agent liveness for `window_id`.
///
/// Deliberately selects the registered local owner rather than a session sink:
/// a session can temporarily have multiple sinks during sharing, but Flow's
/// liveness contract is local-only and unshared.
async fn focused_issue_recipient(
    window_id: WindowId,
    window_shares: &WindowShares,
) -> Option<SharedWriter> {
    let shares = window_shares.read().await;
    let share = shares.get(&window_id)?;
    let local = share.local_participant()?;
    beads_detail_connection_available(Some(share), &local.writer, false)
        .then(|| Arc::clone(&local.writer))
}

struct BeadsDetailRequest<'a> {
    window_id: WindowId,
    writer: &'a SharedWriter,
    is_remote: bool,
    workspace_id: WorkspaceId,
}

async fn beads_detail_request_root(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    request: BeadsDetailRequest<'_>,
) -> Option<PathBuf> {
    let allowed = {
        let shares = window_shares.read().await;
        beads_detail_connection_available(
            shares.get(&request.window_id),
            request.writer,
            request.is_remote,
        )
    };
    if !allowed {
        return None;
    }

    let workspaces = workspace_manager.read().await;
    workspaces
        .window_contains_workspace(request.window_id, request.workspace_id)
        .then(|| workspaces.workspace_info(request.workspace_id))
        .flatten()
        .and_then(|(_, _, _, root)| root)
}

async fn dispatch_window_message(msg: ClientMessage, context: &mut ClientDispatchContext<'_>) {
    match msg {
        ClientMessage::CloseWindow { window_id: target_window } => {
            handle_close_window(target_window, context).await;
        }
        ClientMessage::QuitAll => {
            handle_quit_all(
                context.window_id,
                &context.server.window_shares,
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
        ClientMessage::DismissCiRun { repo_root, head_sha } => {
            dismiss_ci_run(context, repo_root, head_sha).await;
        }
        ClientMessage::SetCiRunDetailsInterest { repo_root, head_sha, interested } => {
            set_ci_detail_interest(context, repo_root, head_sha, interested).await;
        }
        ClientMessage::CheckForUpdates => {
            handle_check_for_updates(context).await;
        }
        ClientMessage::ListReleases => {
            handle_list_releases_msg(context).await;
        }
        ClientMessage::ListWindows => {
            handle_list_windows(
                &context.server.window_shares,
                &context.server.workspace_manager,
                context.writer,
            )
            .await;
        }
        ClientMessage::DispatchAction { window_id: target_window_id, action } => {
            handle_dispatch_action(
                target_window_id,
                action,
                &context.server.window_shares,
                context.window_id,
                context.writer,
            )
            .await;
        }
        ClientMessage::ActionCompleted { correlation_id, outcome, created_session_id } => {
            if !context.server.agent_api.complete_action(
                action_client_key(context.writer),
                correlation_id,
                outcome,
                created_session_id,
            ) {
                debug!(correlation_id, "ignored unknown or stale action completion");
            }
        }
        ClientMessage::AgentPromptResponse { prompt_id, decision } => {
            handle_agent_prompt_response(context, prompt_id, decision).await;
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
            ai_launch: request.ai_launch,
            shell_tool: request.shell_tool,
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

    // Heap-allocated rather than inlined: `start_session` assembles the whole
    // `LiveSession` on its frame, and this call sits inside the long
    // `serve_connection` future, which has to stay under the workspace's
    // future-size budget.
    Box::pin(start_session(
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
            git_ref_watcher: &context.server.git_ref_watcher,
            window_shares: &context.server.window_shares,
        },
    ))
    .await;
    // A created session is attached by `InitialAttachment`, never by
    // `AttachSessions`, so this is the only place its creator can claim the
    // image capability. Without it a client's own bootstrapped shell stays
    // text-only forever and no application running in it is ever answered.
    let _ = admit_image_capable_sessions(vec![session_id], Vec::new(), context).await;
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
/// Build a session's initial attached-sink set from the optional attaching client
/// (feature 015 T007). One sink is byte-identical to the pre-015 `Some(writer)`
/// slot; empty means the session starts detached.
///
/// The initial sink starts `Live`: a freshly created session has no history to
/// replay, so there is no snapshot for its output to race.
async fn initial_client_writer(writer: Option<&SharedWriter>) -> ClientWriter {
    let mut sinks = AttachedSinks::default();
    if let Some(writer) = writer {
        let queue = writer.lock().await.queue();
        sinks.set_sole(Arc::clone(writer), queue);
    }
    Arc::new(std::sync::Mutex::new(sinks))
}

/// The handles a session's two halves — the [`LiveSession`] registry entry and
/// the PTY reader task — both need, created once at startup so
/// [`start_session`] only has to wire them together.
///
/// Everything here is either shared state (cloned into both halves) or one end
/// of a channel that connects them, so building it in one place keeps the
/// split itself readable and keeps [`start_session`] a wiring step.
struct SharedSessionHandles {
    /// The reader half of the session's PTY. Not shared — it is moved straight
    /// into the reader task — but the split that produces it also produces
    /// `pty_write`, so both ends are made here rather than one being made
    /// twice over.
    pty_read: tokio::io::ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    /// Server-owned image capability, cloned into both halves.
    image_sharing: SharedImageSharing,
    client_writer: ClientWriter,
    term_commit: SessionCommit,
    search_cache: SessionSearchCache,
    attachment: SessionAttachment,
    clipboard_command_tx: tokio::sync::mpsc::UnboundedSender<ClipboardCommand>,
    clipboard_command_rx: tokio::sync::mpsc::UnboundedReceiver<ClipboardCommand>,
    preserve_ai_scrollback: Arc<AtomicBool>,
    scrollback_lines: Arc<AtomicUsize>,
    /// Whether a client's last focus report named this session as focused.
    /// Written by `handle_focus_changed`, read by the reader when the
    /// application enables focus reporting (DECSET 1004).
    has_focus: Arc<AtomicBool>,
    exit_gate: Arc<SessionExitGate>,
}

impl SharedSessionHandles {
    async fn new(
        pty_fd: scribe_pty::async_fd::AsyncPtyFd,
        initial_attachment: InitialAttachment<'_>,
    ) -> Self {
        // Seed the session's attached-sink set with the initial client (if any) so
        // the reader task keeps running detached when empty. A single sink here is
        // byte-identical to the pre-015 `Some(writer)` slot.
        let client_writer = initial_client_writer(initial_attachment.writer).await;
        let (pty_read, pty_write) = tokio::io::split(pty_fd);
        let (clipboard_command_tx, clipboard_command_rx) = new_clipboard_command_channel();
        let (preserve_ai_scrollback, scrollback_lines) = load_shared_scrollback_state();
        Self {
            pty_read,
            pty_write: Arc::new(Mutex::new(pty_write)),
            image_sharing: new_session_image_sharing(),
            client_writer,
            term_commit: Arc::default(),
            search_cache: Arc::default(),
            attachment: Arc::new(Mutex::new(initial_attachment.attached_ids.map(Arc::clone))),
            clipboard_command_tx,
            clipboard_command_rx,
            preserve_ai_scrollback,
            scrollback_lines,
            has_focus: Arc::new(AtomicBool::new(false)),
            exit_gate: Arc::new(SessionExitGate::new()),
        }
    }
}

/// A fresh session's image capability: `text-only-unlatched` under the current
/// master switch, until the first capable viewer latches it.
fn new_session_image_sharing() -> SharedImageSharing {
    Arc::new(std::sync::Mutex::new(SessionImageSharing::new(images_master_enabled())))
}

async fn start_session(
    ids: StartSessionIds,
    session: ManagedSession,
    initial_attachment: InitialAttachment<'_>,
    runtime: SessionRuntimeContext<'_>,
) {
    let StartSessionIds { session: session_id, workspace: workspace_id, window: window_id } = ids;
    #[rustfmt::skip]
    let ManagedSession {
        slot, pty_fd, resize_fd, child_pid, child_pidfd, child_identity, term, ansi_processor,
        osc_parser, event_rx, shell_name, pty, handoff_snapshot, task_label, title, icon_title, cwd,
        context, ai_state, ai_provider_hint, shell_tool, prompt_state, cell_width, cell_height,
        env_window_id, env_envelope_id, image_state, ..
    } = session;
    let shared = SharedSessionHandles::new(pty_fd, initial_attachment).await;
    let (terminal_images, terminal_grid_observer) =
        new_session_image_seam(session_id, cell_width, cell_height, image_state.as_deref());
    #[rustfmt::skip]
    let live = LiveSession {
        pty_write: Arc::clone(&shared.pty_write), resize_fd: Arc::new(resize_fd),
        term: Arc::clone(&term), term_commit: Arc::clone(&shared.term_commit),
        terminal_grid_observer: terminal_grid_observer.clone(),
        terminal_images: Arc::clone(&terminal_images), search_cache: Arc::clone(&shared.search_cache),
        child_pid, child_identity,
        client_writer: Arc::clone(&shared.client_writer),
        image_sharing: Arc::clone(&shared.image_sharing), attachment: Arc::clone(&shared.attachment),
        workspace_id, shell_name, title, icon_title, task_label, cwd,
        last_cwd_report: None, git_branch_cache: GitBranchCache::default(),
        context, ai_state, ai_provider_hint, shell_tool, prompt_state, focused_issue: None,
        cell_width, cell_height, resize_pacer: std::sync::Mutex::default(),
        pty, handoff_snapshot,
        preserve_ai_scrollback: Arc::clone(&shared.preserve_ai_scrollback),
        scrollback_lines: Arc::clone(&shared.scrollback_lines), has_focus: Arc::clone(&shared.has_focus),
        env_window_id: env_window_id.unwrap_or(window_id), env_envelope_id,
        clipboard_command_tx: shared.clipboard_command_tx,
        exit_gate: Arc::clone(&shared.exit_gate), _slot: slot,
    };
    let ai_provider = register_session(&runtime, session_id, child_pidfd, child_pid, live).await;
    spawn_pty_reader(
        PtyReaderInputs {
            ids,
            child_pid,
            ai_provider,
            cell_width,
            cell_height,
            pty_read: shared.pty_read,
            pty_write: shared.pty_write,
            term,
            term_commit: shared.term_commit,
            search_cache: shared.search_cache,
            ansi_processor,
            osc_parser,
            event_rx,
            clipboard_command_rx: shared.clipboard_command_rx,
            client_writer: shared.client_writer,
            terminal_images,
            image_sharing: shared.image_sharing,
            attachment: shared.attachment,
            preserve_ai_scrollback: shared.preserve_ai_scrollback,
            scrollback_lines: shared.scrollback_lines,
            has_focus: shared.has_focus,
        },
        runtime,
        &shared.exit_gate,
    );
}

/// Insert the registry entry and arm the child-exit watcher.
///
/// Returns the effective AI provider for the reader. The exit handles are
/// cloned before the insert because the watcher
/// outlives the registry entry and has to finalize the session after it is
/// gone.
async fn register_session(
    runtime: &SessionRuntimeContext<'_>,
    session_id: SessionId,
    child_pidfd: Option<OwnedFd>,
    child_pid: u32,
    live: LiveSession,
) -> Option<AiProvider> {
    let ai_provider = live.ai_state.as_ref().map(|state| state.provider).or(live.ai_provider_hint);
    let exit_handles = live.exit_handles();
    if let Some(cwd) = live.cwd.as_deref() {
        observe_git_repository(runtime.git_ref_watcher, cwd);
    }
    runtime.live_sessions.write().await.insert(session_id, live);
    spawn_child_exit_watcher(child_pidfd, child_pid, session_id, exit_handles, runtime);
    ai_provider
}

/// The `ManagedSession`-derived inputs a PTY reader task needs. Bundled so
/// [`spawn_pty_reader`] owns the derived per-task state (filters, OSC 52
/// bookkeeping, scrollback epoch) instead of inflating [`start_session`].
struct PtyReaderInputs {
    ids: StartSessionIds,
    child_pid: u32,
    ai_provider: Option<AiProvider>,
    cell_width: u16,
    cell_height: u16,
    pty_read: ReadHalf<scribe_pty::async_fd::AsyncPtyFd>,
    pty_write: Arc<Mutex<WriteHalf<scribe_pty::async_fd::AsyncPtyFd>>>,
    term: Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: SessionCommit,
    search_cache: SessionSearchCache,
    ansi_processor: AnsiProcessor,
    osc_parser: VteParser,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    clipboard_command_rx: tokio::sync::mpsc::UnboundedReceiver<ClipboardCommand>,
    client_writer: ClientWriter,
    terminal_images: SessionImageState,
    image_sharing: SharedImageSharing,
    attachment: SessionAttachment,
    preserve_ai_scrollback: Arc<AtomicBool>,
    scrollback_lines: Arc<AtomicUsize>,
    has_focus: Arc<AtomicBool>,
}

/// Build one session's image seam, sized to its cells and carrying whatever a
/// predecessor exported for it.
fn new_session_image_seam(
    session_id: SessionId,
    cell_width: u16,
    cell_height: u16,
    image_state: Option<&crate::terminal_image_handoff::SessionImageHandoff>,
) -> (SessionImageState, TerminalGridObserverHandle) {
    let mut terminal_images = PtyTerminalImageState::new(TerminalImageProcessPolicy::v1());
    let observer = terminal_images.grid_observer();
    observer.set_cell_size(cell_width, cell_height);
    stage_restored_image_state(session_id, &mut terminal_images, image_state);
    (Arc::new(Mutex::new(terminal_images)), observer)
}

/// Install a predecessor's committed image state before this session's reader
/// consumes a single byte.
///
/// A rejected payload is logged and dropped: the seam it was rejected on is
/// still empty, so the session starts imageless rather than half-restored,
/// which is the same bounded degradation an over-ceiling export produces.
// @lat: [[terminal-images#Terminal Images#Image State Across Handoff]]
fn stage_restored_image_state(
    session_id: SessionId,
    terminal_images: &mut PtyTerminalImageState,
    image_state: Option<&crate::terminal_image_handoff::SessionImageHandoff>,
) {
    let Some(state) = image_state else { return };
    // The successor retains no canonical pixels of its own yet, so the
    // reassembled bytes are dropped here exactly as the live publication path
    // drops them; the definitions and placements they back are what the
    // restore installs.
    match terminal_images.restore_handoff(state, &mut |_, _| {}) {
        Ok(()) => {
            let restored = terminal_images.state();
            info!(
                %session_id,
                generation = restored.generation.0,
                sequence = restored.sequence.0,
                definitions = restored.definition_count,
                placements = restored.placement_count,
                "restored terminal image state from handoff"
            );
        }
        Err(error) => warn!(
            %session_id,
            %error,
            "handoff image state refused; the session starts with no images"
        ),
    }
}

/// Assemble the reader task's state and start it, retaining its `JoinHandle`
/// on the session's exit gate (spec 017 US1-3) rather than discarding it, so
/// teardown can stop and join a reader whose master fd will never EOF.
///
/// There is no `.await` between the caller's registry insert and the handle
/// store, so a close can only reach the gate handle-less by racing on another
/// worker — and that close has already cancelled the gate, so the reader ends
/// regardless.
fn spawn_pty_reader(
    inputs: PtyReaderInputs,
    runtime: SessionRuntimeContext<'_>,
    exit_gate: &Arc<SessionExitGate>,
) {
    let state = PtyReaderState {
        session_id: inputs.ids.session,
        window_id: inputs.ids.window,
        child_pid: inputs.child_pid,
        pty_read: inputs.pty_read,
        cancel: exit_gate.subscribe(),
        exit_gate: Arc::clone(exit_gate),
        pty_write: inputs.pty_write,
        term: inputs.term,
        term_commit: inputs.term_commit,
        terminal_images: inputs.terminal_images,
        image_sharing: inputs.image_sharing,
        search_cache: inputs.search_cache,
        ansi_processor: inputs.ansi_processor,
        osc_parser: inputs.osc_parser,
        event_rx: inputs.event_rx,
        client_writer: inputs.client_writer,
        attachment: inputs.attachment,
        workspace_manager: Arc::clone(runtime.workspace_manager),
        live_sessions: Arc::clone(runtime.live_sessions),
        git_ref_watcher: Arc::clone(runtime.git_ref_watcher),
        window_shares: Arc::clone(runtime.window_shares),
        clipboard_burst: ClipboardBurstState::new(load_clipboard_policy_snapshot()),
        pending_clipboard_reads: HashMap::new(),
        pending_clipboard_prompt: None,
        clipboard_command_rx: inputs.clipboard_command_rx,
        osc_events: Vec::new(),
        last_proc_cwd: None,
        ed3_filter: Ed3Filter::new(),
        claude_picker_filter: ClaudePickerTruncationFilter::new(),
        lf_crlf_filter: LfCrlfFilter::new(),
        ai_provider: inputs.ai_provider,
        cell_width: inputs.cell_width,
        cell_height: inputs.cell_height,
        preserve_ai_scrollback: inputs.preserve_ai_scrollback,
        scrollback_lines: inputs.scrollback_lines,
        has_focus: inputs.has_focus,
        focus_mode_was_active: false,
        preserved_ai_scrollback: PreservedAiScrollback::default(),
        pending_ai_scrollback_baseline: false,
        image_evidence: ImageApplicationEvidence::default(),
    };

    exit_gate.set_reader(tokio::spawn(pty_reader_task(state)));
}

/// How long the child-exit watcher lets the PTY drain before it publishes
/// `SessionExited` anyway.
///
/// The reader's read normally fails within microseconds of the child's death,
/// so this only bites when a descendant inherited the slave fd and is holding
/// it open — the case where the master stream may never end at all. Reporting
/// the exit late there beats the old behavior of never reporting it.
const CHILD_EXIT_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Watch a fresh session's child and publish its real exit status
/// (spec 017 US1-2).
///
/// The watcher is the session's authoritative emitter: the end of the master
/// stream only proves every slave fd closed, so [`finalize_pty_reader`] yields
/// the funnel to this task whenever one is armed. It still arbitrates through the same
/// [`SessionExitGate::claim_exit`] CAS, so an explicit close that got there
/// first simply makes this a no-op.
///
/// The child is peeked, never reaped — [`PtyGuard::teardown`] fired from the
/// finalizer below is what runs the `waitpid` that clears the zombie.
fn spawn_child_exit_watcher(
    child_pidfd: Option<OwnedFd>,
    child_pid: u32,
    session_id: SessionId,
    handles: SessionExitHandles,
    runtime: &SessionRuntimeContext<'_>,
) {
    let Some(watcher) = child_pidfd.and_then(|fd| ChildExitWatcher::arm(fd, child_pid)) else {
        return;
    };
    // Armed before the reader starts (this runs on the same synchronous stretch
    // as its spawn): a child that dies immediately must not reach the reader's
    // stream-ended path while the gate still reads as watcher-less, or the
    // session would report an unknown status.
    handles.exit_gate.arm_watcher();
    let live_sessions = Arc::clone(runtime.live_sessions);
    let window_shares = Arc::clone(runtime.window_shares);
    let workspace_manager = Arc::clone(runtime.workspace_manager);
    tokio::spawn(async move {
        let exit = watcher.exited().await;
        // Let the reader finish draining first: the child's death and its last
        // write are independent wakeups, and emitting the exit ahead of the
        // tail output would retire the pane before the client painted it.
        if tokio::time::timeout(CHILD_EXIT_DRAIN_GRACE, handles.exit_gate.reader_finished())
            .await
            .is_err()
        {
            debug!(
                %session_id,
                "PTY still open after the child exited — publishing the exit without a full drain"
            );
        }
        finalize_session_exit(
            &handles.exit_gate,
            SessionExitContext {
                session_id,
                client_writer: &handles.client_writer,
                attachment: &handles.attachment,
                live_sessions: &live_sessions,
                window_shares: &window_shares,
                workspace_manager: &workspace_manager,
            },
            exit,
        )
        .await;
    });
}

/// The pair of config-backed runtime flags a session and its PTY reader share:
/// AI-scrollback preservation and the scrollback row cap. Both are swapped in
/// place by config reloads, so both live behind an `Arc`.
fn load_shared_scrollback_state() -> (Arc<AtomicBool>, Arc<AtomicUsize>) {
    (
        Arc::new(AtomicBool::new(load_preserve_ai_scrollback_setting())),
        Arc::new(AtomicUsize::new(load_scrollback_lines_setting())),
    )
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
    if let Some(lost_id) = lost
        && attached_contains(attached_ids, lost_id).await
        && let Some(session) = sessions.get(&lost_id)
    {
        session.has_focus.store(false, Ordering::Relaxed);
        send_focus_event(session, FOCUS_LOST).await;
    }
    if let Some(gained_id) = gained
        && attached_contains(attached_ids, gained_id).await
        && let Some(session) = sessions.get(&gained_id)
    {
        session.has_focus.store(true, Ordering::Relaxed);
        send_focus_event(session, FOCUS_GAINED).await;
    }
}

/// The wall-clock point a close path stops waiting for its readers.
///
/// `CloseWindow` computes this once and passes it to every session it is
/// closing, so the bound covers the whole window rather than each pane.
fn join_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + READER_JOIN_TIMEOUT
}

/// Step 3 of the close protocol: wait out a cancelled reader, bounded, and log
/// whichever way it went (spec 017 US1-1, US1-3).
///
/// The caller must already have cancelled `gate` and must hold no lock — see
/// [`crate::session_exit`] for why. Nothing here fails a close: a reader that
/// outlives the bound is detached and reported, because the session is already
/// unwired and the task holds nothing the close still needs.
async fn join_reader_bounded(
    session_id: SessionId,
    gate: &SessionExitGate,
    deadline: tokio::time::Instant,
) {
    match gate.join_reader_by(deadline).await {
        ReaderJoin::Joined | ReaderJoin::Absent => {}
        ReaderJoin::Failed(err) => {
            warn!(%session_id, %err, "PTY reader task ended abnormally");
        }
        ReaderJoin::Detached => {
            warn!(
                %session_id,
                bound_ms = READER_JOIN_TIMEOUT.as_millis(),
                "PTY reader did not stop within the close bound — detaching it"
            );
        }
        ReaderJoin::SelfJoin => {
            warn!(%session_id, "reader join requested from the reader task itself — skipped");
        }
    }
}

/// Send `SIGHUP` to the child process of a handoff-restored session, but only
/// once the PID is proven to still name that child (spec 017 US7-2).
///
/// After a hot-reload handoff the `pty` field is `None` because we only
/// received the master fd via `SCM_RIGHTS`, not the original `Pty` object.
/// Without the `Pty` there is no guard to tear down, so nothing sends `SIGHUP`
/// to the child. This helper fills that gap so `CloseSession` and
/// `CloseWindow` can clean up handoff-restored sessions correctly.
///
/// Those children were reparented to init when the old server exited, so init
/// reaps them the instant they die and the PID becomes free without this
/// process ever noticing. A bare `kill` on stored state can therefore land on
/// whatever inherited the number. [`crate::child_identity`] rejects that by
/// comparing the child's recorded start time against the process currently
/// holding the PID; anything short of a match logs and signals nothing, which
/// includes payloads from senders that predate the field. Those sessions still
/// clean up through the reader's EOF path, per the plan's inherited-session
/// exemption.
///
/// The identity read is a couple of procfs syscalls with no I/O wait, which is
/// why it is acceptable on the `CloseWindow` path where the caller still holds
/// the `live_sessions` write guard.
fn signal_if_handoff_session(session_id: SessionId, session: &LiveSession) {
    if session.pty.is_some() {
        return; // `PtyGuard::teardown` will send SIGHUP off-worker.
    }
    let pid = session.child_pid.cast_signed();
    let identity = check_child_identity(session.child_pid, session.child_identity);
    if !identity.may_signal() {
        if identity == IdentityCheck::Recycled {
            warn!(%session_id, pid, "skipping SIGHUP: PID now names a different process");
        } else {
            info!(%session_id, pid, ?identity, "skipping SIGHUP: child identity unproven");
        }
        return;
    }
    info!(%session_id, pid, "sending SIGHUP to handoff-restored session");
    if let Err(err) = kill(Pid::from_raw(pid), Signal::SIGHUP) {
        warn!(%session_id, pid, %err, "failed to send SIGHUP to child");
    }
}

/// Close a session and clean up, following the take-then-release-then-join
/// protocol documented in [`crate::session_exit`] (spec 017 US1-1).
///
/// For fresh sessions [`PtyGuard::teardown`] hands the `Pty` to the blocking
/// pool, where its `Drop` signals and reaps the child off any worker; for
/// handoff-restored sessions (`pty: None`) we send SIGHUP explicitly so the
/// child is not leaked. Neither guarantees an EOF — the child may ignore
/// SIGHUP, and the master fd stays open in the reader and the resize fd
/// regardless — so the close cancels the reader's exit gate, waits out the
/// reader under the [`READER_JOIN_TIMEOUT`] bound, and then drives the same
/// [`finalize_session_exit`] funnel the reader would have, as a backstop for a
/// reader that cannot reach it.
///
/// The join comes after every unwiring step and before the funnel. It has to
/// run with no guard held, because a reader finalizing itself takes both write
/// locks on its way out; putting it last also means a wedged reader delays
/// nothing but the exit notification, and in the ordinary case it lets the
/// reader flush its final bytes ahead of `SessionExited`, since whichever path
/// wins the funnel CAS is the one that emits.
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
    window_shares: &WindowShares,
    attached_ids: &AttachedSessionIds,
) {
    if !attached_contains(attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent CloseSession for unattached session");
        return;
    }

    // Clear the ephemeral agent binding while the session remains in the
    // registry, so its local owner receives the clear before SessionExited.
    set_focused_issue(session_id, None, live_sessions, window_shares).await;

    // Step 1 — take: the registry write guard covers the removal and nothing
    // else, and holds across no `.await`.
    let mut removed = live_sessions.write().await.remove(&session_id);
    // Step 2 — released. Nothing below reacquires it, and the workspace guard
    // is only taken once this one is gone.
    //
    // Capture envelope coordinates before the session value is dropped so
    // we can fire the delete after SIGHUP cleanup. Cloned (rather than
    // moved) to keep the existing `drop(removed)` ordering.
    let envelope_coords = removed
        .as_ref()
        .and_then(|s| s.env_envelope_id.as_ref().map(|id| (s.env_window_id, id.clone())));
    // Cloned for the same reason: the funnel runs after the session value is
    // gone, and it is the only place allowed to emit `SessionExited`.
    let exit_handles = removed.as_ref().map(LiveSession::exit_handles);
    if let Some(session) = &removed {
        signal_if_handoff_session(session_id, session);
    }
    let pty = removed.as_mut().and_then(LiveSession::take_pty);
    drop(removed);

    // Step 3 — cancel, tear down, join. Cancellation is what actually stops a
    // reader whose child ignored SIGHUP; the SIGHUP + `waitpid` behind
    // `teardown` go to the blocking pool so the worker running this close is
    // never parked behind the child.
    if let Some(handles) = &exit_handles {
        handles.exit_gate.cancel();
    }
    if let Some(pty) = pty {
        pty.teardown();
    }
    {
        // Second guard of the documented pair, taken only after the first is
        // gone and released before the join below.
        let mut wm = workspace_manager.write().await;
        wm.remove_session(session_id);
        wm.remove_session_from_window(session_id);
    }
    attached_remove(attached_ids, session_id).await;
    if let Some(handles) = &exit_handles {
        join_reader_bounded(session_id, &handles.exit_gate, join_deadline()).await;
    }

    // Best-effort envelope + DEK delete. `delete_envelope` is idempotent
    // and swallows `NotFound`, so it's safe to call when the feature was
    // off at create time (no envelope ever existed) or when the persist
    // scheduler had not yet flushed a first write. Failures are logged
    // but do not block the close.
    if let Some((window_id, launch_id)) = envelope_coords
        && let Err(err) = crate::env_store::store::delete_envelope(window_id, &launch_id).await
    {
        warn!(
            target: "scribe_server::ipc_server",
            %session_id,
            %window_id,
            %launch_id,
            error = ?err,
            "env-envelope delete failed during CloseSession"
        );
    }

    info!(%session_id, "session closed by client");

    // Funnel last. The cancelled reader normally wins the CAS and emits
    // `SessionExited` itself; this call is the backstop for a reader that
    // cannot get there. It is deliberately after every teardown step, because
    // `SessionExited` goes out on the session's sinks and can park behind a
    // stalled participant — the close itself must not wait on that.
    if let Some(handles) = exit_handles {
        finalize_session_exit(
            &handles.exit_gate,
            SessionExitContext {
                session_id,
                client_writer: &handles.client_writer,
                attachment: &handles.attachment,
                live_sessions,
                window_shares,
                workspace_manager,
            },
            ChildExit::UNKNOWN,
        )
        .await;
    }
}

/// Close a window: destroy every session it owns and remove the window from
/// the workspace manager so it won't be resurrected on the next client launch.
///
/// Per T019, also sweeps every env envelope under the closing window's
/// `restore/env/<window_id>/` directory (and the matching keystore DEKs).
/// This is the "clean close" path; the PTY-EOF / child-exit path in
/// [`finalize_pty_reader`] deliberately preserves envelopes so they remain
/// available for cold-restart restore.
///
/// Each destroyed session's reader is cancelled, joined under the shared
/// [`READER_JOIN_TIMEOUT`] bound, and funnelled through
/// [`finalize_session_exit`] after the `WindowClosed` reply, so a child that
/// ignores SIGHUP cannot leave a reader parked on a master fd that never EOFs.
/// The take-then-release-then-join protocol is documented in
/// [`crate::session_exit`] (spec 017 US1-1).
async fn handle_close_window(window_id: WindowId, context: &ClientDispatchContext<'_>) {
    if window_id != context.window_id {
        send_error(context.writer, &format!("cannot close another window: {window_id}")).await;
        return;
    }

    // The workspace read guard is a statement temporary: it is gone before the
    // registry guard below, so this never inverts the documented lock order.
    let session_ids = context.server.workspace_manager.read().await.sessions_for_window(window_id);
    info!(%window_id, count = session_ids.len(), "closing window — destroying sessions");

    // Session removal below drops the registry field, but emitting the clear
    // first prevents an attached owner from holding a stale live-agent halo.
    for &session_id in &session_ids {
        set_focused_issue(
            session_id,
            None,
            &context.server.live_sessions,
            &context.server.window_shares,
        )
        .await;
    }

    // Step 1 — take: one registry write guard, holding nothing but the
    // removals. The session values leave with it so the per-session work below
    // (which includes the procfs identity read) runs with the global registry
    // free, and so no `.await` can land inside the critical section.
    let mut removed = Vec::with_capacity(session_ids.len());
    {
        let mut sessions = context.server.live_sessions.write().await;
        removed.extend(
            session_ids
                .iter()
                .filter_map(|&sid| sessions.remove(&sid).map(|session| (sid, session))),
        );
    }
    // Step 2 — released. For handoff-restored sessions (`pty: None`) we signal
    // explicitly; fresh sessions keep their `PtyGuard`, torn down below. Exit
    // handles are cloned out for the post-reply teardown, since neither signal
    // guarantees the reader ever sees EOF.
    let mut exit_handles = Vec::with_capacity(removed.len());
    let mut ptys = Vec::with_capacity(removed.len());
    for (sid, mut session) in removed {
        exit_handles.push((sid, session.exit_handles()));
        signal_if_handoff_session(sid, &session);
        ptys.extend(session.take_pty());
    }
    // Every id the window claimed, not just the ones still live, so a session
    // that raced out of the registry still leaves the client's attached set.
    for &sid in &session_ids {
        attached_remove(context.attached_ids, sid).await;
    }
    // Only now do we hand the children to the blocking pool, so a
    // SIGHUP-ignoring child can neither hold the global `live_sessions` write
    // lock nor park a Tokio worker.
    for pty in ptys {
        pty.teardown();
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

    // Step 3 — cancel, then join, then finalize, all after the reply so the
    // pre-existing `WindowClosed`-then-`SessionExited` order the client's exit
    // path expects is preserved. Cancellation is what makes a SIGHUP-ignoring
    // child's reader stop at all; the funnel is the backstop for a reader that
    // cannot get there itself.
    //
    // Every reader is cancelled before any is joined, and all of them share
    // one deadline, so a window of wedged panes costs a single
    // `READER_JOIN_TIMEOUT` rather than one per pane. No guard is held here:
    // a reader finalizing itself takes both write locks on its way out.
    for (_, handles) in &exit_handles {
        handles.exit_gate.cancel();
    }
    let deadline = join_deadline();
    for (sid, handles) in &exit_handles {
        join_reader_bounded(*sid, &handles.exit_gate, deadline).await;
    }
    for (sid, handles) in exit_handles {
        finalize_session_exit(
            &handles.exit_gate,
            SessionExitContext {
                session_id: sid,
                client_writer: &handles.client_writer,
                attachment: &handles.attachment,
                live_sessions: &context.server.live_sessions,
                window_shares: &context.server.window_shares,
                workspace_manager: &context.server.workspace_manager,
            },
            ChildExit::UNKNOWN,
        )
        .await;
    }
}

/// Route a client `Resize` by the window's sharing mode (feature 015 T008, D3). In
/// `SingleController` mode it retains the legacy meaning — the controller-gated
/// direct grid-set via [`handle_resize`]. In a shared mode it is an informational
/// per-participant viewport report: it is stored on the participant for the
/// smallest-wins authoritative grid (consumed by T014) and does NOT drive the PTY
/// winsize directly. With every foundational share built in `SingleController`
/// mode, only the legacy path runs today.
async fn handle_resize_message(
    session_id: SessionId,
    size: TerminalSize,
    context: &ClientDispatchContext<'_>,
) {
    let mode = {
        let shares = context.server.window_shares.read().await;
        shares.get(&context.window_id).map(|share| share.mode)
    };
    match mode {
        Some(
            scribe_config::SharingMode::SharedSingleTypist | scribe_config::SharingMode::FreeForAll,
        ) => {
            store_participant_viewport(context.server, context.window_id, context.writer, size)
                .await;
        }
        _ => {
            handle_resize(session_id, size, &context.server.live_sessions, context.attached_ids)
                .await;
        }
    }
}

/// Store one participant's reported terminal viewport on its `Participant` (feature
/// 015 T008/T014, D3) and schedule a debounced recompute of the smallest-wins
/// authoritative grid. Ignores a zero-dimension report and a writer that is not an
/// attached participant.
async fn store_participant_viewport(
    server: &IpcServerState,
    window_id: WindowId,
    writer: &SharedWriter,
    size: TerminalSize,
) {
    if !size.has_grid() {
        return;
    }
    let Some((debounce, generation)) =
        record_viewport_report(&server.window_shares, window_id, writer, size).await
    else {
        return;
    };
    // Coalesce reports over the debounce window before applying (D3). The min is a
    // pure function of the current viewports, so concurrent reports converge to the
    // same settled grid, and only the report that armed this timer spawns one — a
    // drag's worth of reports settles to a single trailing apply (#24).
    let server = server.clone();
    tokio::spawn(async move {
        if await_settled_viewport_reports(&server.window_shares, window_id, debounce, generation)
            .await
        {
            apply_authoritative_grid(&server, window_id).await;
        }
    });
}

/// Store one report on its participant and take the arming decision under a single
/// share lock. `None` when the report is dropped (window or participant gone) or
/// when a trailing-apply timer is already armed for this window.
async fn record_viewport_report(
    window_shares: &WindowShares,
    window_id: WindowId,
    writer: &SharedWriter,
    size: TerminalSize,
) -> Option<(std::time::Duration, u64)> {
    let mut shares = window_shares.write().await;
    let share = shares.get_mut(&window_id)?;
    let participant = share.participant_for_writer_mut(writer)?;
    participant.viewport = size;
    share.grid.arm_trailing_apply()
}

/// Wait out the debounce window, restarting it for every report that lands while
/// the timer sleeps, and report whether the window still exists once the reports
/// settle. `false` means the share went away mid-wait and there is nothing to
/// apply; its arming state died with it.
async fn await_settled_viewport_reports(
    window_shares: &WindowShares,
    window_id: WindowId,
    debounce: std::time::Duration,
    armed_generation: u64,
) -> bool {
    let mut observed = armed_generation;
    loop {
        tokio::time::sleep(debounce).await;
        let mut shares = window_shares.write().await;
        let Some(share) = shares.get_mut(&window_id) else {
            return false;
        };
        match share.grid.settle_trailing_apply(observed) {
            Some(newer) => observed = newer,
            None => return true,
        }
    }
}

/// Recompute a shared window's smallest-wins authoritative grid (feature 015 T014,
/// FR-012) and, when it changed, drive the PTY winsize for every session in the
/// window once. `SingleController` windows are skipped — they keep the legacy
/// direct grid-set. A no-op when the min is unchanged; regrow is inherent (a
/// departed participant's viewport simply drops out of the min).
async fn apply_authoritative_grid(server: &IpcServerState, window_id: WindowId) {
    let target = {
        let mut shares = server.window_shares.write().await;
        let Some(share) = shares.get_mut(&window_id) else {
            return;
        };
        if matches!(share.mode, scribe_config::SharingMode::SingleController) {
            return;
        }
        let Some((rows, cols)) = share.smallest_viewport() else {
            return;
        };
        if share.grid.rows == rows && share.grid.cols == cols {
            return;
        }
        share.grid.rows = rows;
        share.grid.cols = cols;
        (rows, cols)
    };
    debug!(%window_id, rows = target.0, cols = target.1, "authoritative grid applied");
    apply_grid_to_window_sessions(server, window_id, target.0, target.1).await;
}

/// Drive `resize_term` + `set_pty_winsize` (`TIOCSWINSZ`) for every live session in
/// `window_id` to `rows × cols`, reusing each session's last-known cell pixel size
/// (feature 015 T014).
async fn apply_grid_to_window_sessions(
    server: &IpcServerState,
    window_id: WindowId,
    rows: u16,
    cols: u16,
) {
    let session_ids = server.workspace_manager.read().await.sessions_for_window(window_id);
    for session_id in session_ids {
        let handles = {
            let sessions = server.live_sessions.read().await;
            sessions.get(&session_id).map(|s| {
                (
                    Arc::clone(&s.term),
                    Arc::clone(&s.resize_fd),
                    s.terminal_grid_observer.clone(),
                    s.cell_width,
                    s.cell_height,
                )
            })
        };
        let Some((term, resize_fd, terminal_grid_observer, cell_width, cell_height)) = handles
        else {
            continue;
        };
        terminal_grid_observer.set_cell_size(cell_width, cell_height);
        let reflowed = resize_term(&term, &terminal_grid_observer, cols, rows).await;
        let size = TerminalSize { cols, rows, cell_width, cell_height };
        if let Err(e) = set_pty_winsize(resize_fd.as_ref(), size) {
            warn!(%session_id, "authoritative-grid TIOCSWINSZ failed: {e}");
        }
        note_unpaced_resize_apply(session_id, &server.live_sessions).await;
        // The recompute is debounced, so this apply lands well after the
        // participants' own `RequestSnapshot`s were answered at the pre-reflow
        // grid; without a push they would render that stale grid forever.
        if reflowed {
            broadcast_post_resize_snapshot(session_id, &server.live_sessions).await;
        }
    }
}

/// Resize the terminal and PTY, paced to at most four applies per second.
///
/// The cell size is recorded from every report — it only feeds winsize replies
/// and costs nothing — but the reflow itself goes through the session's
/// [`ResizePacer`] (spec 017 US7-3), so a drag's report stream collapses into a
/// leading apply plus one per [`RESIZE_APPLY_INTERVAL`], ending at the size the
/// drag stopped on.
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

    let admission = {
        let mut sessions = live_sessions.write().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            warn!(%session_id, "Resize for unknown session");
            return;
        };
        session.cell_width = size.cell_width.max(1);
        session.cell_height = size.cell_height.max(1);
        lock_resize_pacer(&session.resize_pacer).admit(size, std::time::Instant::now())
    };

    match admission {
        ResizeAdmission::ApplyNow => {
            // No push: the client pairs every `Resize` with a `RequestSnapshot`,
            // and this apply completes before that request is dispatched.
            apply_session_resize(session_id, size, live_sessions).await;
        }
        ResizeAdmission::Coalesced => {}
        ResizeAdmission::Arm(delay) => {
            let live_sessions = Arc::clone(live_sessions);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                apply_trailing_resize(session_id, &live_sessions).await;
            });
        }
    }
}

/// Tell a session's [`ResizePacer`] that a grid apply just ran outside it — the
/// attach-time resize and the shared-window authoritative grid both drive
/// `resize_term` directly rather than through an admission.
///
/// Without this the pacer keeps state describing a grid that is no longer on
/// screen: a size held from before the direct apply can still mature over it,
/// and a `last_apply` stamp older than the direct apply lets the next report
/// buy an immediate extra reflow.
pub async fn note_unpaced_resize_apply(session_id: SessionId, live_sessions: &LiveSessionRegistry) {
    let sessions = live_sessions.read().await;
    if let Some(session) = sessions.get(&session_id) {
        lock_resize_pacer(&session.resize_pacer).note_external_apply(std::time::Instant::now());
    }
}

/// Apply whatever size a session's armed trailing timer is holding, once its
/// window has elapsed. A session that left the registry mid-drag has nothing to
/// apply, which is how a close cancels the timer the drag armed.
///
/// Registry presence is not on its own enough to make the held size current: a
/// detach or a direct apply that landed after the timer was armed clears the
/// pending size ([`ResizePacer::discard_pending`],
/// [`ResizePacer::note_external_apply`]), so this finds nothing and the newer
/// grid stands.
async fn apply_trailing_resize(session_id: SessionId, live_sessions: &LiveSessionRegistry) {
    let pending = {
        let sessions = live_sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            return;
        };
        lock_resize_pacer(&session.resize_pacer).take_pending(std::time::Instant::now())
    };
    let Some(size) = pending else { return };
    // A trailing apply is by definition deferred: the client's `RequestSnapshot`
    // for this report was answered back when the pacer was still holding it, at
    // the pre-reflow grid. Hand the repaint back now that the grid is real.
    if apply_session_resize(session_id, size, live_sessions).await {
        broadcast_post_resize_snapshot(session_id, live_sessions).await;
    }
}

/// Drive one session's `Term` reflow and `TIOCSWINSZ` at `size` — the work both
/// the leading and the trailing apply do. Returns whether the reflow actually
/// moved the grid, which is what tells a deferred caller it owes a repaint.
async fn apply_session_resize(
    session_id: SessionId,
    size: TerminalSize,
    live_sessions: &LiveSessionRegistry,
) -> bool {
    let (term, resize_fd, terminal_grid_observer) = {
        let sessions = live_sessions.read().await;
        let Some(session) = sessions.get(&session_id) else {
            warn!(%session_id, "Resize for unknown session");
            return false;
        };
        (
            Arc::clone(&session.term),
            Arc::clone(&session.resize_fd),
            session.terminal_grid_observer.clone(),
        )
    };

    // Resize the Term state (lock + drop before any await).
    terminal_grid_observer.set_cell_size(size.cell_width, size.cell_height);
    let reflowed = resize_term(&term, &terminal_grid_observer, size.cols, size.rows).await;

    // Signal the PTY with TIOCSWINSZ.
    if let Err(e) = set_pty_winsize(resize_fd.as_ref(), size) {
        warn!(%session_id, "TIOCSWINSZ failed: {e}");
    }
    reflowed
}

/// Push the grid a deferred resize just applied to every sink attached to
/// `session_id`.
///
/// A client asks for the authoritative screen at the moment it reports a new
/// pane size, but a deferred apply — the [`ResizePacer`]'s trailing edge, or the
/// shared-window authoritative-grid debounce — reflows the `Term` long after
/// that request was answered. The reply the client already has therefore
/// describes a grid the server has abandoned, and since the client only asks
/// again on its *own* next size change, a drag that ends leaves the pane
/// permanently rendering stale geometry. Pushing the post-apply screen is what
/// closes that window.
///
/// Leading applies deliberately push nothing: they land ahead of the
/// `RequestSnapshot` that the same report carries, so that reply is already at
/// matching geometry and a push would only duplicate it.
async fn broadcast_post_resize_snapshot(
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) {
    let handles = {
        let sessions = live_sessions.read().await;
        sessions.get(&session_id).map(|session| {
            (
                Arc::clone(&session.term),
                Arc::clone(&session.term_commit),
                Arc::clone(&session.client_writer),
            )
        })
    };
    let Some((term, term_commit, client_writer)) = handles else { return };
    if lock_sinks(&client_writer).is_empty() {
        return;
    }

    // The same compressed, self-resetting replay used for attach is the bounded
    // repair frame here. A per-cell ScreenSnapshot can exceed MAX_MESSAGE_SIZE
    // with the configured 10,000-row scrollback and never reach the pane.
    let (replay, commit) = match crate::attach_flow::take_session_replay(
        session_id,
        &term,
        &term_commit,
        live_sessions,
    )
    .await
    {
        Ok(replay) => replay,
        Err(error) => {
            warn!(%session_id, %error, "post-resize replay build failed");
            return;
        }
    };
    debug!(%session_id, cols = replay.cols, rows = replay.rows, "pushed post-resize replay");
    send_to_client(
        &client_writer,
        Some(commit),
        &ServerMessage::SessionReplay { session_id, replay },
    );
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
    let term_commit = Arc::clone(&session.term_commit);
    let cache = Arc::clone(&session.search_cache);
    drop(sessions);

    // Only the snapshot needs the `Term` (spec 017 US8-1). The scan is a
    // read of owned data, and the PTY reader task needs this same mutex for
    // every chunk it feeds, so holding it across the scan would stall the
    // session's own output path once per keystroke.
    //
    // The snapshot itself is taken once per query burst (spec 017 US8-2): while
    // the grid stands still, later edits validate the cached picture under the
    // lock and reuse it, so a 10-character query holds the `Term` for one
    // snapshot plus nine key comparisons instead of ten snapshots.
    let snapshot = {
        let term_guard = term.lock().await;
        let key = SnapshotKey {
            commit: term_commit.get(),
            cols: grid_dimension(term_guard.grid().columns()),
            rows: grid_dimension(term_guard.grid().screen_lines()),
            scrollback_rows: u32::try_from(term_guard.grid().history_size()).unwrap_or(u32::MAX),
        };
        cache.get(key).unwrap_or_else(|| {
            let snapshot = Arc::new(snapshot_term(&term_guard));
            cache.store(key, Arc::clone(&snapshot));
            snapshot
        })
    };
    let matches = search_snapshot(&snapshot, &query, limit);

    let msg = ServerMessage::SearchResults { session_id, query, matches };
    send_message(context.writer, &msg).await;
}

/// Narrow a grid extent to the `u16` the wire snapshot carries, saturating so an
/// absurd geometry compares equal to itself rather than wrapping.
fn grid_dimension(extent: usize) -> u16 {
    u16::try_from(extent).unwrap_or(u16::MAX)
}

/// Release the scrollback snapshot cached for a session whose find overlay just
/// closed (spec 017 US8-2).
///
/// Advisory: the reader drops the same entry on the session's next output, so a
/// client that dies with its overlay open costs at most one snapshot until then.
async fn handle_search_closed(session_id: SessionId, context: &ClientDispatchContext<'_>) {
    if !attached_contains(context.attached_ids, session_id).await {
        tracing::warn!(%session_id, "client sent SearchClosed for unattached session");
        return;
    }

    let sessions = context.server.live_sessions.read().await;
    if let Some(session) = sessions.get(&session_id) {
        session.search_cache.invalidate();
    }
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

/// Lock the `Term` and apply the new dimensions, reporting whether the grid
/// actually moved.
///
/// The deferred apply paths use that answer to decide whether they owe attached
/// clients a repaint: a reflow that changed nothing leaves every client's own
/// `RequestSnapshot` answer still correct, so re-pushing the screen would be a
/// redundant full repaint (scribe-k9o).
pub async fn resize_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    terminal_grid_observer: &TerminalGridObserverHandle,
    cols: u16,
    rows: u16,
) -> bool {
    // Guard dropped at the end of the body — before any subsequent .await.
    let mut term_guard = term.lock().await;
    let before = (term_guard.columns(), term_guard.screen_lines());
    let size = ResizeDimensions { cols: usize::from(cols), lines: usize::from(rows) };
    term_guard.resize(size);
    let changed = before != (term_guard.columns(), term_guard.screen_lines());
    observe_terminal_resize(terminal_grid_observer, &*term_guard, changed);
    changed
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

/// Handle `RequestSnapshot` with the bounded whole-pane replay clients already
/// use for attach and overflow repair.
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

    let handles = {
        let sessions = live_sessions.read().await;
        sessions
            .get(&session_id)
            .map(|session| (Arc::clone(&session.term), Arc::clone(&session.term_commit)))
    };
    let Some((term, term_commit)) = handles else {
        send_error(writer, &format!("RequestSnapshot for unknown session {session_id}")).await;
        return;
    };

    let replay = match crate::attach_flow::take_session_replay(
        session_id,
        &term,
        &term_commit,
        live_sessions,
    )
    .await
    {
        Ok((replay, _)) => replay,
        Err(error) => {
            send_error(writer, &format!("RequestSnapshot repair failed for {session_id}: {error}"))
                .await;
            return;
        }
    };
    send_message(writer, &ServerMessage::SessionReplay { session_id, replay }).await;
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
fn list_session_launch_id(
    is_remote: bool,
    requested_window: WindowId,
    envelope_window: WindowId,
    launch_id: Option<&str>,
) -> Option<String> {
    launch_id.filter(|_| !is_remote && requested_window == envelope_window).map(str::to_owned)
}

async fn handle_list_sessions(
    live_sessions: &LiveSessionRegistry,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
    window_id: WindowId,
    is_remote: bool,
) {
    let sessions = live_sessions.read().await;
    let wm = workspace_manager.read().await;

    // Filter sessions to those belonging to this window.
    let window_session_ids = wm.sessions_for_window(window_id);
    let has_window_sessions = !window_session_ids.is_empty();

    // Branch resolution is split around the guards: probing the per-session
    // memo is a path compare and a clone, cheap enough to do here, while a
    // miss costs a `.git/HEAD` walk and must not run under the registry and
    // workspace-manager read guards. Misses are collected and resolved once
    // both guards are released.
    let mut pending_branches: Vec<(SessionId, std::path::PathBuf)> = Vec::new();
    let mut build_info = |sid: SessionId, s: &LiveSession| SessionInfo {
        session_id: sid,
        workspace_id: s.workspace_id,
        launch_id: list_session_launch_id(
            is_remote,
            window_id,
            s.env_window_id,
            s.env_envelope_id.as_deref(),
        ),
        shell_name: s.shell_name.clone(),
        title: s.title.clone(),
        icon_title: s.icon_title.clone(),
        context: s.context.clone(),
        task_label: s.task_label.clone(),
        codex_task_label: s.task_label.clone(),
        cwd: s.cwd.clone(),
        git_branch: s.cwd.as_deref().and_then(|cwd| match s.git_branch_cache.fresh(cwd) {
            BranchLookup::Hit(branch) => branch,
            BranchLookup::Miss => {
                pending_branches.push((sid, cwd.to_path_buf()));
                None
            }
        }),
        ai_state: s.ai_state.clone(),
        ai_provider_hint: s.ai_state.as_ref().map(|state| state.provider).or(s.ai_provider_hint),
        shell_tool: s.shell_tool,
        prompt_state: s.prompt_state.clone(),
    };

    let mut infos: Vec<SessionInfo> = if has_window_sessions {
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

    if !pending_branches.is_empty() {
        let resolved = resolve_pending_git_branches(&pending_branches, live_sessions).await;
        for info in &mut infos {
            if let Some(branch) = resolved.get(&info.session_id) {
                info.git_branch.clone_from(branch);
            }
        }
    }

    let list_msg = ServerMessage::SessionList { sessions: infos, workspace_tree, workspaces };
    send_message(writer, &list_msg).await;
}

/// Snapshot the controller writer of every connected window's share — the fan-out
/// target for server-wide broadcasts (`QuitRequested` and updater notices). In
/// `SingleController` mode each share has one participant, so
/// this is byte-identical to iterating the pre-015 `connected_clients` values.
pub async fn connected_window_writers(window_shares: &WindowShares) -> Vec<SharedWriter> {
    window_shares
        .read()
        .await
        .values()
        .filter_map(|share| share.controller_writer().cloned())
        .collect()
}

/// Snapshot each owning-machine participant for a local-only lifecycle action.
async fn connected_local_window_writers(window_shares: &WindowShares) -> Vec<SharedWriter> {
    window_shares
        .read()
        .await
        .values()
        .filter_map(|share| {
            share.local_participant().map(|participant| Arc::clone(&participant.writer))
        })
        .collect()
}

/// Publish one tracker delta to capable participants whose window contains the repo.
/// A dismissed head stays hidden; publishing a different head clears its dismissal.
pub async fn publish_ci_run_delta(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    dismissals: &CiDismissals,
    repo_root: &Path,
    delta: CiRunDelta,
) {
    {
        let mut dismissed = dismissals.write().await;
        match &delta {
            CiRunDelta::Set(state)
                if dismissed.get(repo_root).is_some_and(|head| head == &state.head_sha) =>
            {
                return;
            }
            CiRunDelta::Set(_) => {
                dismissed.remove(repo_root);
            }
            CiRunDelta::Cleared { .. } => {}
        }
    }
    send_ci_run_delta(workspace_manager, window_shares, repo_root, delta).await;
}

async fn send_ci_run_delta(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    repo_root: &Path,
    delta: CiRunDelta,
) {
    let windows = workspace_manager.read().await.windows_for_project_root(repo_root);
    let writers = {
        let shares = window_shares.read().await;
        windows
            .iter()
            .filter_map(|window_id| shares.get(window_id))
            .flat_map(WindowShare::ci_run_writers)
            .collect::<Vec<_>>()
    };
    let message = ServerMessage::CiRunState { repo_root: repo_root.to_path_buf(), delta };
    for writer in &writers {
        send_message(writer, &message).await;
    }
}

struct CiDismissRequest<'a> {
    window_id: WindowId,
    writer: &'a SharedWriter,
    is_remote: bool,
    repo_root: PathBuf,
    head_sha: String,
}

async fn dismiss_ci_run(context: &ClientDispatchContext<'_>, repo_root: PathBuf, head_sha: String) {
    apply_ci_dismissal(
        &context.server.workspace_manager,
        &context.server.window_shares,
        &context.server.ci_dismissals,
        CiDismissRequest {
            window_id: context.window_id,
            writer: context.writer,
            is_remote: context.is_remote,
            repo_root,
            head_sha,
        },
    )
    .await;
}

async fn set_ci_detail_interest(
    context: &ClientDispatchContext<'_>,
    repo_root: PathBuf,
    head_sha: String,
    interested: bool,
) {
    if !ci_detail_interest_allowed(
        &context.server.workspace_manager,
        &context.server.window_shares,
        CiDetailInterestRequest {
            window_id: context.window_id,
            writer: context.writer,
            repo_root: &repo_root,
            interested,
        },
    )
    .await
    {
        debug!(window_id = %context.window_id, ?repo_root, "ignoring unauthorized CI detail interest");
        return;
    }
    context.server.github_ci_tracker.set_detail_interest(DetailInterest {
        repo_root,
        head_sha,
        writer: Arc::clone(context.writer),
        interested,
    });
}

struct CiDetailInterestRequest<'a> {
    window_id: WindowId,
    writer: &'a SharedWriter,
    repo_root: &'a Path,
    interested: bool,
}

async fn ci_detail_interest_allowed(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    request: CiDetailInterestRequest<'_>,
) -> bool {
    let capable = window_shares
        .read()
        .await
        .get(&request.window_id)
        .and_then(|share| share.participant_for_writer(request.writer))
        .is_some_and(|participant| participant.ci_run_bar);
    capable
        && (!request.interested
            || workspace_manager
                .read()
                .await
                .window_contains_project_root(request.window_id, request.repo_root))
}

/// Accept an owning capable client's dismissal and synchronize it across views.
async fn apply_ci_dismissal(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    dismissals: &CiDismissals,
    request: CiDismissRequest<'_>,
) {
    if request.is_remote {
        debug!(window_id = %request.window_id, "ignoring CI dismissal from a remote viewer");
        return;
    }
    let capable = window_shares
        .read()
        .await
        .get(&request.window_id)
        .and_then(|share| share.participant_for_writer(request.writer))
        .is_some_and(|participant| participant.ci_run_bar);
    if !capable
        || !workspace_manager
            .read()
            .await
            .window_contains_project_root(request.window_id, &request.repo_root)
    {
        debug!(window_id = %request.window_id, repo_root = ?request.repo_root, "ignoring unauthorized CI dismissal");
        return;
    }

    let delta = CiRunDelta::Cleared { head_sha: request.head_sha.clone() };
    dismissals.write().await.insert(request.repo_root.clone(), request.head_sha);
    publish_ci_run_delta(workspace_manager, window_shares, dismissals, &request.repo_root, delta)
        .await;
}

/// Feature 015 (T022, D8): broadcast the full-state `ShareRoster` to every
/// participant of a shared window on each membership / control change. A no-op in
/// `SingleController` mode — that mode never emits v3 roster frames, preserving
/// parity — and when the window has no share. The roster and the target writers are
/// snapshotted under the read lock, then sent with NO lock held (D5 — a send never
/// awaits a socket under the share lock).
async fn broadcast_share_roster(server: &IpcServerState, window_id: WindowId) {
    let snapshot = {
        let shares = server.window_shares.read().await;
        let Some(share) = shares.get(&window_id) else {
            return;
        };
        if matches!(share.mode, scribe_config::SharingMode::SingleController) {
            return;
        }
        let msg = ServerMessage::ShareRoster {
            window_id,
            participants: share.roster(),
            mode: share.mode,
            holder: share.holder_id(),
        };
        (msg, share.all_writers())
    };
    let (msg, writers) = snapshot;
    for writer in &writers {
        send_message(writer, &msg).await;
    }
}

/// Feature 015 (T023, FR-015): record a share-membership change on the existing 013
/// remote-audit surface (`REMOTE_AUDIT_TARGET`) so SC-007's 100% coverage holds.
/// `event` is `join` / `leave` / `control-transfer` / `control-request` /
/// `control-denied`; the participant is named by its control-transition label
/// (`local`, or `device (login)`), matching the feature-013 taxonomy.
fn audit_membership_event(window_id: WindowId, event: &str, identity: &ControllerIdentity) {
    info!(
        target: REMOTE_AUDIT_TARGET,
        "share: {event} window={window_id} participant={}",
        identity.transition_label()
    );
}

/// The post-lock side effects of a control-transfer message (feature 015
/// T017–T019). Resolved under the share write lock (state mutated there), then
/// applied — audit + broadcast + targeted sends — with no lock held (D5).
enum ControlEffect {
    /// Not applicable / stale / already-holder — no change, no message.
    None,
    /// The holder transferred: audit + broadcast the new roster.
    Transferred { holder: ControllerIdentity },
    /// A request-and-grant request was recorded: notify each approver.
    Requested { approvers: Vec<SharedWriter>, from: ParticipantInfo, requester: ControllerIdentity },
    /// A request was denied (or its requester vanished): notify the requester.
    Denied { requester_writer: SharedWriter, requester: ControllerIdentity },
}

/// Feature 015 (T017/T018): resolve a `ControlClaim` / `ControlRequest` from
/// `writer` against a shared window's control state, mutating it under the caller's
/// write lock and returning the effects to apply after the lock drops. Only
/// meaningful in `SharedSingleTypist`. Under `FreeClaim` any attached participant
/// takes control instantly; under `RequestAndGrant` the owner (FR-007) or an unheld
/// share yields instantly, and a non-owner claim of a held share becomes a pending
/// request routed to the current holder and the owner.
fn resolve_control_acquire(share: &mut WindowShare, writer: &SharedWriter) -> ControlEffect {
    if !matches!(share.mode, scribe_config::SharingMode::SharedSingleTypist) {
        return ControlEffect::None;
    }
    let Some(claimant) = share.participant_id_for_writer(writer) else {
        return ControlEffect::None;
    };
    if share.holder_id() == Some(claimant) {
        return ControlEffect::None; // already holds control
    }
    let is_owner = share.local_participant().is_some_and(|p| p.id == claimant);
    let held = share.holder_id().is_some();
    let instant = match share.control_acquisition {
        scribe_config::ControlAcquisition::FreeClaim => true,
        scribe_config::ControlAcquisition::RequestAndGrant => is_owner || !held,
    };
    if instant {
        // Transfer control; the previous holder stays attached as a live viewer.
        share.control =
            ControlState::SingleTypist { holder: Some(claimant), pending_request: None };
        let holder = share
            .participants
            .get(&claimant)
            .map_or(ControllerIdentity::Local, |p| p.identity.clone());
        return ControlEffect::Transferred { holder };
    }
    // Request-and-grant, non-owner, held share: record the pending request and
    // route `ControlRequested` to the current holder and the owner (either may
    // grant, FR-005/FR-007).
    let Some(from) = share.participant_info(claimant) else {
        return ControlEffect::None;
    };
    let requester =
        share.participants.get(&claimant).map_or(ControllerIdentity::Local, |p| p.identity.clone());
    let approvers = approver_writers(share);
    if approvers.is_empty() {
        return ControlEffect::None;
    }
    if let ControlState::SingleTypist { pending_request, .. } = &mut share.control {
        *pending_request = Some(PendingRequest { requester: claimant });
    }
    ControlEffect::Requested { approvers, from, requester }
}

/// The writers authorized to grant a pending request: the current holder and the
/// local owner (de-duplicated by `Arc::ptr_eq`), per FR-005/FR-007.
fn approver_writers(share: &WindowShare) -> Vec<SharedWriter> {
    let mut writers: Vec<SharedWriter> = Vec::new();
    if let Some(holder) = share.holder_id()
        && let Some(p) = share.participants.get(&holder)
    {
        writers.push(Arc::clone(&p.writer));
    }
    if let Some(owner) = share.local_participant()
        && !writers.iter().any(|w| Arc::ptr_eq(w, &owner.writer))
    {
        writers.push(Arc::clone(&owner.writer));
    }
    writers
}

/// Feature 015 (T018): resolve a `ControlGrant` from `writer`. Honored only from
/// the named approver (the current holder or the local owner) and only against the
/// current pending request's requester. `accept` transfers control; otherwise the
/// request is cleared and the requester is denied.
fn resolve_control_grant(
    share: &mut WindowShare,
    writer: &SharedWriter,
    participant_id: ParticipantId,
    accept: bool,
) -> ControlEffect {
    if !matches!(share.mode, scribe_config::SharingMode::SharedSingleTypist) {
        return ControlEffect::None;
    }
    let granter = share.participant_id_for_writer(writer);
    let is_holder = granter.is_some() && granter == share.holder_id();
    let is_owner = share.local_participant().map(|p| p.id) == granter;
    if !(is_holder || is_owner) {
        return ControlEffect::None; // not an authorized approver
    }
    let pending = match &share.control {
        ControlState::SingleTypist { pending_request: Some(r), .. } => Some(r.requester),
        _ => None,
    };
    if pending != Some(participant_id) {
        return ControlEffect::None; // stale / no matching request
    }
    // Clear the pending request regardless of the decision.
    if let ControlState::SingleTypist { pending_request, .. } = &mut share.control {
        *pending_request = None;
    }
    let requester_identity = share.participants.get(&participant_id).map(|p| p.identity.clone());
    let requester_writer = share.participants.get(&participant_id).map(|p| Arc::clone(&p.writer));
    match (accept, requester_identity, requester_writer) {
        (true, Some(holder), _) => {
            share.control =
                ControlState::SingleTypist { holder: Some(participant_id), pending_request: None };
            ControlEffect::Transferred { holder }
        }
        (false, Some(requester), Some(requester_writer)) => {
            ControlEffect::Denied { requester_writer, requester }
        }
        // Requester vanished between request and grant — request already cleared.
        _ => ControlEffect::None,
    }
}

/// Apply a resolved [`ControlEffect`] after the share lock has dropped: audit the
/// membership change and fan out the roster / targeted notice (feature 015
/// T017–T019/T022/T023).
async fn apply_control_effect(server: &IpcServerState, window_id: WindowId, effect: ControlEffect) {
    match effect {
        ControlEffect::None => {}
        ControlEffect::Transferred { holder } => {
            audit_membership_event(window_id, "control-transfer", &holder);
            broadcast_share_roster(server, window_id).await;
        }
        ControlEffect::Requested { approvers, from, requester } => {
            audit_membership_event(window_id, "control-request", &requester);
            let msg = ServerMessage::ControlRequested { window_id, from };
            for writer in &approvers {
                send_message(writer, &msg).await;
            }
        }
        ControlEffect::Denied { requester_writer, requester } => {
            audit_membership_event(window_id, "control-denied", &requester);
            send_message(&requester_writer, &ServerMessage::ControlDenied { window_id }).await;
        }
    }
}

/// Dispatch the v3 control-transfer messages (feature 015 T017/T018). Each resolves
/// under one share write lock, then applies its effects with no lock held.
async fn handle_control_message(msg: ClientMessage, context: &ClientDispatchContext<'_>) {
    let (window_id, effect) = {
        let mut shares = context.server.window_shares.write().await;
        match msg {
            ClientMessage::ControlClaim { window_id }
            | ClientMessage::ControlRequest { window_id } => {
                let effect = shares
                    .get_mut(&window_id)
                    .map_or(ControlEffect::None, |s| resolve_control_acquire(s, context.writer));
                (window_id, effect)
            }
            ClientMessage::ControlGrant { window_id, participant_id, accept } => {
                let effect = shares.get_mut(&window_id).map_or(ControlEffect::None, |s| {
                    resolve_control_grant(s, context.writer, participant_id, accept)
                });
                (window_id, effect)
            }
            _ => return,
        }
    };
    apply_control_effect(context.server, window_id, effect).await;
}

/// The post-lock side effects of applying a live mode change to one active share
/// (feature 015 T032). Resolved under the `window_shares` write lock (state mutated
/// there), then applied — sends + audit — with no lock held (D5).
struct ModeChangeAction {
    window_id: WindowId,
    /// Remote participants detached by a `→ SingleController` flip: each is sent
    /// `WindowTakenOver` + `ShareEnded { ModeChangedToSingleController }` and its
    /// sink is removed from the window's sessions.
    detached: Vec<SharedWriter>,
    /// A cancelled pending request's requester, to inform with `ControlDenied`.
    denied: Option<SharedWriter>,
    /// Whether to re-broadcast the roster (every transition that leaves the share
    /// in a shared mode).
    broadcast: bool,
}

/// Feature 015 (T032, FR-017): reconcile every ACTIVE share to `snapshot.mode`
/// immediately on a live mode change. Mutates each share under one write lock, then
/// applies the sends/audit with no lock held (D5).
async fn reconcile_shares_for_mode_change(server: &IpcServerState, snapshot: SharingSnapshot) {
    let actions = {
        let mut shares = server.window_shares.write().await;
        shares
            .iter_mut()
            .filter_map(|(window_id, share)| {
                apply_mode_change_to_share(*window_id, share, snapshot)
            })
            .collect::<Vec<_>>()
    };
    for action in actions {
        let window_id = action.window_id;
        for writer in &action.detached {
            // The owning machine ("this machine") is now the sole controller.
            send_message(writer, &ControllerIdentity::Local.window_taken_over()).await;
            send_message(
                writer,
                &ServerMessage::ShareEnded {
                    window_id,
                    reason: scribe_common::protocol::ShareEndReason::ModeChangedToSingleController,
                },
            )
            .await;
            detach_writer_from_window_sessions(server, window_id, writer).await;
        }
        if let Some(denied) = &action.denied {
            send_message(denied, &ServerMessage::ControlDenied { window_id }).await;
        }
        if action.broadcast {
            broadcast_share_roster(server, window_id).await;
        }
        audit_membership_event(window_id, "mode-change", &ControllerIdentity::Local);
    }
}

/// Apply a live mode change to one share under the caller's write lock, returning
/// the effects to apply after the lock drops — or `None` when the share is already
/// in the target mode (a no-op reload must not drop control or demote anyone). Also
/// refreshes the share's `control_acquisition` / `participant_limit` snapshots.
fn apply_mode_change_to_share(
    window_id: WindowId,
    share: &mut WindowShare,
    snapshot: SharingSnapshot,
) -> Option<ModeChangeAction> {
    share.control_acquisition = snapshot.control_acquisition;
    share.participant_limit = snapshot.participant_limit;
    if share.mode == snapshot.mode {
        return None; // already in the target mode; nothing to reconcile
    }
    // Cancel any pending request across the transition (spec Edge Case), capturing
    // the requester to inform.
    let denied = match &share.control {
        ControlState::SingleTypist { pending_request: Some(r), .. } => {
            share.participants.get(&r.requester).map(|p| Arc::clone(&p.writer))
        }
        _ => None,
    };
    let mut action = ModeChangeAction { window_id, detached: Vec::new(), denied, broadcast: false };
    share.mode = snapshot.mode;
    match snapshot.mode {
        scribe_config::SharingMode::SharedSingleTypist => {
            // Demote all participants to viewers; control unheld and claimable.
            share.control = ControlState::SingleTypist { holder: None, pending_request: None };
            action.broadcast = true;
        }
        scribe_config::SharingMode::FreeForAll => {
            share.control = ControlState::FreeForAll;
            action.broadcast = true;
        }
        scribe_config::SharingMode::SingleController => {
            // Detach every remote participant; the owner retains sole control.
            let remotes: Vec<ParticipantId> = share
                .participants
                .iter()
                .filter(|(_, p)| matches!(p.transport, ParticipantTransport::Remote))
                .map(|(id, _)| *id)
                .collect();
            for id in remotes {
                if let Some(p) = share.participants.remove(&id) {
                    action.detached.push(p.writer);
                }
            }
            if let Some(owner) = share.local_participant() {
                share.control = ControlState::LegacyExclusive { writer: Arc::clone(&owner.writer) };
            }
            // No roster broadcast — `SingleController` emits no v3 roster; the
            // detached remotes get `ShareEnded` instead.
        }
    }
    Some(action)
}

/// Remove one connection's sink from every session of a window (feature 015 T032) —
/// used when a mode flip detaches a remote participant so its sessions stop fanning
/// output to it.
async fn detach_writer_from_window_sessions(
    server: &IpcServerState,
    window_id: WindowId,
    writer: &SharedWriter,
) {
    let session_ids = server.workspace_manager.read().await.sessions_for_window(window_id);
    let sessions = server.live_sessions.read().await;
    for session_id in session_ids {
        if let Some(session) = sessions.get(&session_id) {
            lock_sinks(&session.client_writer).detach(writer);
        }
    }
}

/// Handle `AttachSessions` — take ownership of detached sessions, set the
/// client writer, and send back session + workspace info for each.
async fn handle_attach_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    context: &mut ClientDispatchContext<'_>,
) {
    // Feature 015 (T012): authorize the attach and learn whether to add the sink
    // additively (shared mode, a viewer joins) or replace it (SingleController /
    // legacy takeover re-point). A denied connection attaches nothing.
    let Some(mode) =
        connection_may_attach(&context.server.window_shares, context.window_id, context.writer)
            .await
    else {
        debug!(
            window_id = %context.window_id,
            "AttachSessions denied: connection may not attach to this window"
        );
        return;
    };
    let additive = !matches!(mode, scribe_config::SharingMode::SingleController);

    let (session_ids, dimensions) =
        filter_attachable_sessions(session_ids, dimensions, context).await;
    let (session_ids, dimensions) =
        admit_image_capable_sessions(session_ids, dimensions, context).await;
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
            additive,
        },
    )
    .await;
    attached_extend(context.attached_ids, attached).await;
}

/// Apply the image capability contract to one attach batch.
///
/// A capable viewer latches an unlatched session — that is what makes a
/// created-then-attached session image-enabled. A viewer lacking what a session
/// already latched is refused for that session with the typed mismatch instead
/// of being attached to a screen whose graphics it cannot draw; the rest of its
/// batch still attaches. Zero-viewer retention is unaffected: nothing here
/// clears a latch.
// @lat: [[terminal-images#Terminal Images#Incapable Viewer Refusal]]
async fn admit_image_capable_sessions(
    session_ids: Vec<SessionId>,
    dimensions: Vec<TerminalSize>,
    context: &ClientDispatchContext<'_>,
) -> (Vec<SessionId>, Vec<TerminalSize>) {
    if session_ids.is_empty() {
        return (session_ids, dimensions);
    }
    let viewer = context.writer.lock().await.queue().image_capabilities();
    let include_dimensions = !dimensions.is_empty();
    let mut allowed_ids = Vec::with_capacity(session_ids.len());
    let mut allowed_dimensions = Vec::with_capacity(dimensions.len());
    let mut refusals = Vec::new();

    let sessions = context.server.live_sessions.read().await;
    for (idx, session_id) in session_ids.into_iter().enumerate() {
        let refusal = sessions.get(&session_id).and_then(|session| {
            let mut sharing = lock_image_sharing(&session.image_sharing);
            match sharing.admit(viewer) {
                Ok(()) => {
                    sharing.latch(viewer);
                    None
                }
                Err(mismatch) => Some(mismatch),
            }
        });
        if let Some(mismatch) = refusal {
            refusals.push((session_id, mismatch));
            continue;
        }
        allowed_ids.push(session_id);
        if include_dimensions {
            allowed_dimensions.push(dimensions.get(idx).copied().unwrap_or_default());
        }
    }
    drop(sessions);

    for (session_id, mismatch) in refusals {
        warn!(
            %session_id,
            window_id = %context.window_id,
            "AttachSessions refused: viewer cannot render this session's latched images"
        );
        send_message(
            context.writer,
            &ServerMessage::TerminalImageCapabilityMismatch { session_id, mismatch },
        )
        .await;
    }

    (allowed_ids, allowed_dimensions)
}

async fn filter_attachable_sessions(
    session_ids: &[SessionId],
    dimensions: &[TerminalSize],
    context: &ClientDispatchContext<'_>,
) -> (Vec<SessionId>, Vec<TerminalSize>) {
    // The batch-level authorization (controller in `SingleController`, any
    // participant in a shared mode) is applied by the caller via
    // `connection_may_attach`; here we only drop sessions that belong to a
    // DIFFERENT window than this connection's.
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

    // Drop the cached config + theme snapshot first so every reader below —
    // and the dynamic color-query path — resolves against the new file.
    scribe_config::invalidate_config_snapshot();

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

    server.agent_api.refresh_policy(cfg.agent_api.clone());

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
    apply_image_master_switch(cfg.images_enabled, &sessions);
    crate::github_ci::set_github_ci_enabled(cfg.github_ci.enabled);
    let github_ci_changed = server.git_ref_watcher.set_enabled(cfg.github_ci.enabled);
    let github_ci_cwds: Vec<_> = if github_ci_changed && cfg.github_ci.enabled {
        sessions.values().filter_map(|session| session.cwd.clone()).collect()
    } else {
        Vec::new()
    };
    apply_reload_to_sessions(&sessions, &term_config, new_scrollback, &cfg).await;
    let sessions_len = sessions.len();
    drop(sessions);

    for cwd in github_ci_cwds {
        observe_git_repository(&server.git_ref_watcher, &cwd);
    }

    for (client_writer, msg) in workspace_messages {
        send_to_client(&client_writer, None, &msg);
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

/// Spec 020: mirror `terminal.images.enabled` into the process-wide master
/// switch and into every live session's capability latch.
///
/// The process switch decides what the next connection or session is told; the
/// per-session write decides what an already-latched session does from now on.
/// Disabling unlatches, which is what stops advertising, PTY replies, and
/// fan-out immediately — including for a session with no viewer at all.
/// Re-enabling deliberately restores nothing: a capable viewer must latch
/// again, so an application is never told images came back without a renderer
/// behind them.
///
/// Retained bytes and committed scenes belong to each session's PTY reader,
/// which releases them when it observes the switch; this function only reports
/// how many sessions owe that release.
// @lat: [[terminal-images#Terminal Images#Image Master Switch]]
fn apply_image_master_switch(enabled: bool, sessions: &HashMap<SessionId, LiveSession>) -> usize {
    if set_images_master_enabled(enabled) == enabled {
        return 0;
    }
    let releasing = sessions
        .values()
        .filter(|session| {
            lock_image_sharing(&session.image_sharing).set_master_enabled(enabled).releases_state()
        })
        .count();
    info!(
        images_enabled = enabled,
        sessions = sessions.len(),
        releasing,
        "terminal image master switch changed"
    );
    releasing
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
/// baseline-ready `EnvChanged`. Just emit a marker log. In practice that
/// event comes from the next *newly started* shell: a shell already
/// running emitted its baseline while the feature was off and had it
/// dropped at the gate, and there is no server→shell trigger to ask for
/// another. "Restart or re-init required" is the semantic, not a gap.
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
/// Window enumeration unions the share-registry keys and
/// `workspace_manager::window_ids_with_sessions` (same merge
/// `handle_list_windows` uses) so disconnected windows that still own live
/// sessions are not skipped.
async fn handle_quit_all(
    sender_window_id: WindowId,
    window_shares: &WindowShares,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
) {
    info!(%sender_window_id, "QuitAll requested — broadcasting QuitRequested");

    // Compute the union of live windows before any client teardown can
    // mutate workspace state. Read locks here only — no async work under
    // them — so the order matches `handle_list_windows`.
    let window_ids: HashSet<WindowId> = {
        let shares = window_shares.read().await;
        let wm = workspace_manager.read().await;
        let mut ids: HashSet<WindowId> = wm.window_ids_with_sessions();
        ids.extend(shares.keys().copied());
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

    let quit_msg = ServerMessage::QuitRequested;
    for writer in connected_window_writers(window_shares).await {
        send_message(&writer, &quit_msg).await;
    }
}

async fn handle_list_windows(
    window_shares: &WindowShares,
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    writer: &SharedWriter,
) {
    let shares = window_shares.read().await;
    let wm = workspace_manager.read().await;

    let mut window_ids: HashSet<WindowId> = wm.window_ids_with_sessions();
    window_ids.extend(shares.keys().copied());

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
            connected: shares.contains_key(&window_id),
            workspace_names: wm.workspace_names_for_window(window_id),
            controller: shares
                .get(&window_id)
                .and_then(WindowShare::controller_identity)
                .and_then(ControllerIdentity::to_controller_info),
            // Feature 015 (T022/T026): share occupancy for the connect picker —
            // the remote participants, the window's mode, and the total participant
            // count. Absent (empty / None / 0) for an unconnected window.
            participants: shares
                .get(&window_id)
                .map(WindowShare::remote_controller_infos)
                .unwrap_or_default(),
            mode: shares.get(&window_id).map(|share| share.mode),
            participant_count: shares.get(&window_id).map_or(0, |share| share.participants.len()),
        })
        .collect();
    windows.sort_by_key(|info| info.window_id.to_full_string());
    drop(wm);
    drop(shares);

    send_message(writer, &ServerMessage::WindowList { windows }).await;
}

async fn handle_dispatch_action(
    requested_window_id: Option<WindowId>,
    action: AutomationAction,
    window_shares: &WindowShares,
    sender_window_id: WindowId,
    writer: &SharedWriter,
) {
    if let Some(window_id) = requested_window_id
        && window_id != sender_window_id
    {
        send_error(writer, &format!("cannot dispatch action to another window: {window_id}")).await;
        return;
    }

    let shares = window_shares.read().await;
    let requested_window_id = requested_window_id.unwrap_or(sender_window_id);
    let target_window_id = shares.contains_key(&requested_window_id).then_some(requested_window_id);

    let target_writer = target_window_id
        .and_then(|window_id| shares.get(&window_id))
        .and_then(|share| share.controller_writer().cloned());
    drop(shares);

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

/// Route a local one-shot CLI action without registering or impersonating a
/// window. Unlike a post-`Hello` client, the transient caller has no window of
/// its own: an explicit target is allowed, while omission is unambiguous only
/// when exactly one window is connected.
async fn handle_transient_dispatch_action(
    requested_window_id: Option<WindowId>,
    action: AutomationAction,
    window_shares: &WindowShares,
    writer: &SharedWriter,
) {
    let shares = window_shares.read().await;
    let target_window_id = match requested_window_id {
        Some(window_id) if shares.contains_key(&window_id) => Some(window_id),
        Some(window_id) => {
            drop(shares);
            send_error(writer, &format!("window not connected: {window_id}")).await;
            return;
        }
        None if shares.is_empty() => {
            drop(shares);
            send_error(writer, "cannot dispatch action: no connected windows").await;
            return;
        }
        None if shares.len() > 1 => {
            let count = shares.len();
            drop(shares);
            send_error(
                writer,
                &format!("cannot dispatch action without --window: {count} connected windows"),
            )
            .await;
            return;
        }
        None => shares.keys().next().copied(),
    };

    let Some(target_window_id) = target_window_id else {
        drop(shares);
        send_error(writer, "cannot dispatch action: no connected windows").await;
        return;
    };
    let target_writer =
        shares.get(&target_window_id).and_then(|share| share.controller_writer().cloned());
    drop(shares);

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

struct AgentActionContext<'a> {
    state: &'a crate::agent_api::AgentApiState,
    caller: usize,
    window_shares: &'a WindowShares,
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
}

async fn run_agent_action(
    context: AgentActionContext<'_>,
    requested_window_id: Option<WindowId>,
    action: AutomationAction,
    origin_session_id: Option<SessionId>,
) -> Result<scribe_common::agent::AgentActionResult, scribe_common::agent::AgentError> {
    let (target_window_id, target_writer) = {
        let shares = context.window_shares.read().await;
        let target_window_id = if let Some(window_id) = requested_window_id {
            window_id
        } else {
            let mut window_ids = shares.keys().copied();
            match (window_ids.next(), window_ids.next()) {
                (Some(window_id), None) => window_id,
                _ => {
                    return Err(scribe_common::agent::AgentError::AmbiguousTarget {
                        message: format!(
                            "action target omitted with {} connected windows",
                            shares.len()
                        ),
                    });
                }
            }
        };
        let writer = shares
            .get(&target_window_id)
            .and_then(WindowShare::controller_writer)
            .cloned()
            .ok_or_else(|| scribe_common::agent::AgentError::NotFound {
                message: format!("window {target_window_id} not connected"),
            })?;
        (target_window_id, writer)
    };

    let affected_session = {
        let workspaces = context.workspace_manager.read().await;
        affected_action_session(&workspaces, target_window_id, &action, origin_session_id)
    };
    let _activity = affected_session
        .map(|session_id| context.state.activity().acquire(session_id, context.caller));

    context
        .state
        .run_correlated_action(
            action_client_key(&target_writer),
            action,
            AGENT_ACTION_COMPLETION_TIMEOUT,
            |message| async move { try_send_message(&target_writer, &message).await },
        )
        .await
}

fn affected_action_session(
    workspace_manager: &WorkspaceManager,
    target_window_id: WindowId,
    action: &AutomationAction,
    origin_session_id: Option<SessionId>,
) -> Option<SessionId> {
    let session_id = match action {
        AutomationAction::FocusSession { session_id } => *session_id,
        _ => origin_session_id?,
    };
    (workspace_manager.window_for_session(session_id) == Some(target_window_id))
        .then_some(session_id)
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

/// Select one owning-machine client that advertised agent prompt support.
/// Stable window ordering keeps concurrent callers from spraying the same
/// prompt across several windows; the policy engine correlates the reply.
async fn first_local_agent_api_writer(window_shares: &WindowShares) -> Option<SharedWriter> {
    window_shares
        .read()
        .await
        .iter()
        .filter_map(|(window_id, share)| {
            share.local_agent_api_writer().map(|writer| (window_id, writer))
        })
        .min_by_key(|(window_id, _)| window_id.to_full_string())
        .map(|(_, writer)| writer)
}

/// Spec 027 transient one-shot: route an `AgentRequest` through the agent
/// dispatcher and reply on the same connection, which then closes without
/// registering a window or attaching a session. The registry seams are
/// closures over this server's live-session, share, and workspace state;
/// they run only after the dispatcher's policy gate admits the request.
async fn handle_transient_agent_request(
    server: &IpcServerState,
    writer: &SharedWriter,
    request: &scribe_common::agent::AgentRequest,
) {
    let prompt_writer = first_local_agent_api_writer(&server.window_shares).await;
    let live_sessions = Arc::clone(&server.live_sessions);
    let world_sessions = Arc::clone(&server.live_sessions);
    let world_shares = Arc::clone(&server.window_shares);
    let world_workspaces = Arc::clone(&server.workspace_manager);
    let agent_api = server.agent_api.clone();
    let action_shares = Arc::clone(&server.window_shares);
    let action_workspaces = Arc::clone(&server.workspace_manager);
    let action_caller = action_client_key(writer);
    let action_origin = match request {
        scribe_common::agent::AgentRequest::DispatchAction { origin_session_id, .. } => {
            *origin_session_id
        }
        _ => None,
    };
    let dispatch = Box::pin(crate::agent_api::dispatch(
        &server.agent_api,
        action_caller,
        request,
        crate::agent_api::DispatchSources {
            capture_world: move || async move {
                crate::agent_api::world::capture(
                    &world_sessions,
                    &world_shares,
                    &world_workspaces,
                    copy_live_session_for_agent,
                )
                .await
            },
            lookup_session: move |session_id| async move {
                let sessions = live_sessions.read().await;
                sessions.get(&session_id).map(|session| crate::agent_api::AgentSessionTarget {
                    term: Arc::clone(&session.term),
                    pty_write: Arc::clone(&session.pty_write),
                    title: session.title.clone(),
                    cwd: session.cwd.clone(),
                })
            },
            run_action: move |window_id, action| async move {
                run_agent_action(
                    AgentActionContext {
                        state: &agent_api,
                        caller: action_caller,
                        window_shares: &action_shares,
                        workspace_manager: &action_workspaces,
                    },
                    window_id,
                    action,
                    action_origin,
                )
                .await
            },
        },
        prompt_writer.map(|prompt_client| {
            move |message| async move { send_message(&prompt_client, &message).await }
        }),
    ))
    .await;
    send_message(writer, &ServerMessage::AgentResponse(dispatch.response().clone())).await;
}

/// Copy one live session's allowlisted metadata for an agent world capture.
/// Defined here rather than in `agent_api::world` so `LiveSession`'s private
/// fields — including retained prompt state — stay private to this module.
/// The workspace-manager mapping names the session's window; a session not
/// yet assigned there falls back to its stable creation-time owner.
fn copy_live_session_for_agent(
    session_id: SessionId,
    session: &LiveSession,
    window: Option<WindowId>,
) -> crate::agent_api::world::CapturedSession {
    crate::agent_api::world::CapturedSession {
        session_id,
        window_id: window.unwrap_or(session.env_window_id),
        workspace_id: session.workspace_id,
        title: session.title.clone(),
        cwd: session.cwd.clone(),
        ai_state: session.ai_state.clone(),
        ai_provider_hint: session.ai_provider_hint,
        task_label: session.task_label.clone(),
    }
}

async fn connection_supports_agent_api(
    window_shares: &WindowShares,
    window_id: WindowId,
    writer: &SharedWriter,
) -> bool {
    window_shares
        .read()
        .await
        .get(&window_id)
        .and_then(|share| share.participant_for_writer(writer))
        .is_some_and(|participant| participant.agent_api == AgentApiCapability::Supported)
}

async fn handle_agent_prompt_response(
    context: &ClientDispatchContext<'_>,
    prompt_id: scribe_common::protocol::PromptId,
    decision: scribe_common::protocol::ClipboardDecision,
) {
    if context.is_remote
        || !connection_supports_agent_api(
            &context.server.window_shares,
            context.window_id,
            context.writer,
        )
        .await
    {
        debug!(?prompt_id, "ignored agent prompt response from an incapable client");
    } else if !context.server.agent_api.resolve_prompt(prompt_id, decision) {
        debug!(?prompt_id, "ignored unknown or stale agent prompt response");
    }
}

/// Send an agent-only frame to participants that advertised `agent_api`.
///
/// Keeping the capability filter beside the participant registry makes every
/// agent activity/prompt broadcast fail closed for old clients.
async fn send_agent_api_message(
    window_shares: &WindowShares,
    window_id: WindowId,
    msg: &ServerMessage,
) {
    let writers = window_shares
        .read()
        .await
        .get(&window_id)
        .map(WindowShare::agent_api_writers)
        .unwrap_or_default();
    for writer in writers {
        send_message(&writer, msg).await;
    }
}

/// Hand one `ServerMessage` to a connection's bounded output queue. Never blocks
/// on the socket: overflow is absorbed inside the queue, so this returns `false`
/// only when the connection is already closed.
async fn try_send_message(writer: &SharedWriter, msg: &ServerMessage) -> bool {
    writer.lock().await.0.enqueue(msg)
}

fn action_client_key(writer: &SharedWriter) -> usize {
    Arc::as_ptr(writer).cast::<()>() as usize
}

/// Fan a `ServerMessage` out to every sink attached to a session (feature 015 T007,
/// D1). No-op when detached (the set is empty).
///
/// `commit` is the [`TermCommit`] cursor value that already includes this frame's
/// effect on the shared `Term`, or `None` for frames a replay snapshot cannot
/// carry (metadata, exit, workspace naming). Sinks still awaiting their attach
/// replay buffer the frame against that cursor; `Live` sinks enqueue it. Nothing
/// here awaits, so the per-session set's lock is never held across a sink send
/// and no participant can back-pressure the PTY path.
fn send_to_client(client_writer: &ClientWriter, commit: Option<u64>, msg: &ServerMessage) {
    lock_sinks(client_writer).fan_out(commit, msg);
}

/// Fan typed image records out to the capable sinks only, returning how many
/// viewers received them. Zero viewers is a normal, non-error outcome: an
/// image-latched session keeps parsing and retaining state while unwatched.
pub fn send_image_records(
    client_writer: &ClientWriter,
    session_id: SessionId,
    required: TerminalImageCapabilities,
    messages: &[ServerMessage],
) -> usize {
    lock_sinks(client_writer).fan_out_images(session_id, required, messages)
}

/// How many capable sinks currently owe this session a combined image replay.
pub fn image_replay_debt(
    client_writer: &ClientWriter,
    required: TerminalImageCapabilities,
) -> usize {
    lock_sinks(client_writer).image_replay_debt(required)
}

/// Deliver one planned combined replay burst to every capable sink that owes
/// one, clearing its debt. The burst is planned once regardless of how many
/// sinks receive it.
pub fn send_image_replay(
    client_writer: &ClientWriter,
    required: TerminalImageCapabilities,
    records: &[ServerMessage],
) -> usize {
    let degraded = quota_exceeded_image_replay(records);
    lock_sinks(client_writer).fan_out_image_replay(required, records, &degraded)
}

fn quota_exceeded_image_replay(records: &[ServerMessage]) -> Vec<ServerMessage> {
    let Some(ServerMessage::TerminalImageReplay {
        session_id,
        message: TerminalImageReplayMessage::Begin { generation, after_sequence, active_screen, .. },
    }) = records.first()
    else {
        return Vec::new();
    };
    let observed_bytes = u64::try_from(keep_batch_cost(records)).unwrap_or(u64::MAX);
    terminal_image_replay::quota_exceeded_replay(
        *generation,
        *after_sequence,
        active_screen.unwrap_or(TerminalScreenKind::Primary),
        observed_bytes,
    )
    .into_iter()
    .map(|message| ServerMessage::TerminalImageReplay { session_id: *session_id, message })
    .collect()
}

/// Install an attaching connection's sink on a session in the buffering state,
/// before its replay snapshot is taken (see [`AttachedSinks::begin_attach`]).
///
/// The connection's enqueue handle is resolved before the per-session set is
/// locked, so the only `.await` here happens outside that critical section.
pub async fn begin_sink_attach(
    client_writer: &ClientWriter,
    writer: &SharedWriter,
    additive: bool,
) {
    let queue = writer.lock().await.queue();
    lock_sinks(client_writer).begin_attach(writer, queue, additive);
}

/// Release a buffering sink once its replay is on the wire (see
/// [`AttachedSinks::finish_attach`]).
pub fn finish_sink_attach(
    client_writer: &ClientWriter,
    writer: &SharedWriter,
    snapshot_commit: u64,
    session_id: SessionId,
) {
    lock_sinks(client_writer).finish_attach(writer, snapshot_commit, session_id);
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

    let stop = loop {
        match next_pty_read_action(&mut state, &mut buf).await {
            PtyReadAction::Continue => {}
            PtyReadAction::End(stop) => break stop,
            PtyReadAction::Data(bytes_read) => {
                let Some(bytes) = buf.get(..bytes_read) else { break ReaderStop::ReadError };
                process_pty_chunk(&mut state, bytes).await;
            }
        }
    };

    finalize_pty_reader(state, stop).await;
}

/// Why the reader loop stopped. Decides which exit path the reader takes into
/// the session exit funnel (spec 017 US1-3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReaderStop {
    /// The PTY master reported EOF — every slave fd is closed.
    Eof,
    /// The master fd errored, so no further output can arrive.
    ReadError,
    /// Teardown cancelled this reader; the canceller drives the funnel.
    Cancelled,
}

impl ReaderStop {
    /// Whether the master stream itself ended, rather than the reader being
    /// told to stop.
    ///
    /// Both variants mean the same thing here: Linux answers a read on a
    /// master whose last slave closed with `EIO`, not a zero-length read, so
    /// the common "the shell exited" case arrives as `ReadError`. Neither
    /// proves the child is dead, which is why an armed child-exit watcher
    /// owns the exit status for both.
    const fn ends_master_stream(self) -> bool {
        matches!(self, Self::Eof | Self::ReadError)
    }
}

enum PtyReadAction {
    Continue,
    Data(usize),
    End(ReaderStop),
}

async fn next_pty_read_action(state: &mut PtyReaderState, buf: &mut [u8]) -> PtyReadAction {
    let Some(read_result) = select_pty_read_or_clipboard(state, buf).await else {
        return PtyReadAction::Continue;
    };

    match read_result {
        ReadResult::Data(n) => PtyReadAction::Data(n),
        ReadResult::Eof => PtyReadAction::End(ReaderStop::Eof),
        ReadResult::Cancelled => PtyReadAction::End(ReaderStop::Cancelled),
        ReadResult::Err(e) => {
            warn!(session_id = %state.session_id, "PTY read error: {e}");
            PtyReadAction::End(ReaderStop::ReadError)
        }
    }
}

/// Race a PTY read against the reader's cancellation signal, the optional ANSI
/// sync-timeout sleep, and the OSC 52 [`ClipboardCommand`] channel. Returns
/// `Some(read_result)` when the PTY produced bytes (or an error/EOF) or the
/// reader was cancelled, and `None` when either the sync timeout fired or a
/// clipboard command was consumed — both of those paths are "continue the
/// outer loop" signals for [`next_pty_read_action`].
///
/// Every branch is cancel-safe (`AsyncReadExt::read`,
/// [`next_clipboard_command`], `sleep_until`, and the gate's `watch`-backed
/// waiter), so losing a race never drops a wakeup or a byte.
async fn select_pty_read_or_clipboard(
    state: &mut PtyReaderState,
    buf: &mut [u8],
) -> Option<ReadResult> {
    if let Some(deadline) = state.ansi_processor.sync_timeout().sync_timeout() {
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(sleep);
        tokio::select! {
            () = &mut sleep => {
                let observer = state.terminal_images.lock().await.grid_observer();
                let observation = stop_term_sync(
                    &state.term,
                    &observer,
                    &mut state.ansi_processor,
                )
                .await;
                state.terminal_images.lock().await.record_grid_observation(&observation);
                maybe_capture_preserved_ai_scrollback_baseline(state).await;
                None
            }
            () = state.cancel.cancelled() => Some(ReadResult::Cancelled),
            result = read_pty_bytes(&mut state.pty_read, buf) => Some(result),
            cmd = next_clipboard_command(&mut state.clipboard_command_rx) => {
                handle_clipboard_command(state, cmd).await;
                None
            }
        }
    } else {
        tokio::select! {
            () = state.cancel.cancelled() => Some(ReadResult::Cancelled),
            result = read_pty_bytes(&mut state.pty_read, buf) => Some(result),
            cmd = next_clipboard_command(&mut state.clipboard_command_rx) => {
                handle_clipboard_command(state, cmd).await;
                None
            }
        }
    }
}

/// Yield the next OSC 52 [`ClipboardCommand`], parking forever once the
/// channel closes (spec 017 US1-3).
///
/// The only sender is [`LiveSession::clipboard_command_tx`], so the channel
/// closes the moment the registry entry is dropped — which happens while the
/// reader is still running on every path that ends a session from outside it:
/// a `CloseWindow`, whose cancel lands only after the reply, and the
/// child-exit watcher, whose reader may hold a master that never ends because
/// a descendant still owns the slave. `recv` on a closed receiver completes
/// instantly and forever, which turned that arm into a hot loop burning a core
/// per orphaned reader. Parking makes it go quiet and leaves stopping the
/// reader to cancellation. Cancel-safe, like the `recv` it wraps: buffered
/// commands are still drained before the close is observed.
async fn next_clipboard_command(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClipboardCommand>,
) -> ClipboardCommand {
    match rx.recv().await {
        Some(cmd) => cmd,
        None => std::future::pending().await,
    }
}

/// Apply one `ClipboardCommand`. Shared by the reader's `select!` arm and the
/// between-chunks drain in [`process_metadata_events`].
async fn handle_clipboard_command(state: &mut PtyReaderState, cmd: ClipboardCommand) {
    match cmd {
        ClipboardCommand::PromptResponse { request_id, decision } => {
            handle_clipboard_prompt_response(state, request_id, decision).await;
        }
        ClipboardCommand::BridgeReadReply { request_id, payload } => {
            handle_clipboard_bridge_read_reply(state, request_id, payload).await;
        }
        ClipboardCommand::RefreshPolicy { policy } => {
            handle_clipboard_policy_refresh(state, policy);
            // Spec 020: the `ConfigReloaded` that broadcast this refresh is
            // also where the image master switch can flip, and it reaches
            // every live session. The PTY reader owns the image seam
            // exclusively, so the release has to happen on this task — which
            // this arm already woke.
            release_images_if_disabled(state).await;
        }
    }
}

/// Drop every image resource this session still holds once the master switch
/// is off.
///
/// Cancels outstanding decode admissions, discards partial framing, releases
/// retained buffers, and clears committed definitions and placements, so a
/// disabled Scribe cannot replay a scene to a later viewer or keep pixels
/// resident. The seam itself skips a session that holds nothing, so a config
/// reload across many text-only sessions costs one predicate each.
///
/// Nothing is published and no PTY reply is written — a disabled session has
/// no capable sinks and owes no discovery answer. The terminal's text is
/// untouched, so the application's own textual fallback keeps rendering.
// @lat: [[terminal-images#Terminal Images#Image Master Switch]]
async fn release_images_if_disabled(state: &mut PtyReaderState) {
    if images_master_enabled() {
        return;
    }
    let mut images = state.terminal_images.lock().await;
    let held = images.state();
    match images.release_for_policy_disable() {
        Ok(None) => {}
        Ok(Some(_)) => info!(
            session_id = %state.session_id,
            definitions = held.definition_count,
            placements = held.placement_count,
            "released terminal image state after the master switch went off"
        ),
        Err(error) => warn!(
            session_id = %state.session_id,
            %error,
            "terminal image release after disable failed"
        ),
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
    // The trim already ran against the Term and advanced the commit cursor past
    // itself, so `trimmed_commit` is the cursor value an attach snapshot must be
    // at or beyond for the trim to be redundant.
    let trimmed = if suppressed_ed3 { handle_suppressed_ai_ed3(state).await } else { None };

    if let Some((rows, trimmed_commit)) = trimmed {
        send_trim_scrollback(&state.client_writer, state.session_id, rows, trimmed_commit);
    }

    // The chunk's own cursor value: what the counter reaches once step 2 feeds it
    // into the Term. Stamping the client-bound frames with it BEFORE the feed is
    // safe because the reader task is the only writer of this session's cursor.
    let chunk_commit = state.term_commit.get().saturating_add(chunk_len(effective.as_ref()));

    if capture_baseline_after_feed {
        state.pending_ai_scrollback_baseline = true;
    }

    // Advance the reader-owned image seam before ordinary delivery, then feed
    // every effective byte exactly once through the same real Alacritty Term.
    // The observer splits processor calls only at completed image boundaries;
    // client delivery remains one unchanged read.
    let session_id = state.session_id;
    let images_handle = Arc::clone(&state.terminal_images);
    let mut images = images_handle.lock().await;
    let client_writer = &state.client_writer;
    let term = &state.term;
    let term_commit = &state.term_commit;
    let search_cache = &state.search_cache;
    let ansi_processor = &mut state.ansi_processor;
    let image_result = process_pty_reader_ingress(
        &mut images,
        effective.as_ref(),
        |delivered_bytes| {
            // Step 1: Fast path — forward filtered bytes to the UI client.
            send_pty_output(client_writer, session_id, delivered_bytes, chunk_commit);
        },
        |observer, delivered_bytes, image_result| {
            feed_term_image_result_observed(
                term,
                term_commit,
                search_cache,
                ansi_processor,
                ObservedImageResultFeed { observer, bytes: delivered_bytes, image_result },
            )
        },
        |rejection| {
            warn!(
                session_id = %session_id,
                error = %rejection.error,
                image_sequence = rejection.image_sequence.0,
                "terminal image state rejected PTY chunk"
            );
        },
    )
    .await;
    // Everything below reborrows the seam through `state`, so the ingress
    // guard has to be surrendered first.
    drop(images);

    // Step 2: State path — the shared ingress seam fed filtered bytes into
    // Term. Image sequence rejection never suppresses ordinary delivery.
    if let Ok(image_commit) = image_result {
        // Step 2a: answer the PTY and fan typed records to capable viewers
        // BEFORE `process_metadata_events` drains the Term's own event queue.
        // That single ordering is what puts a Kitty result ahead of the DA1
        // reply an application requests immediately behind its probe.
        deliver_image_commit(state, &image_commit).await;
    }

    maybe_capture_preserved_ai_scrollback_baseline(state).await;

    // Step 2b: a prompt returning while mouse-reporting modes are still
    // active means the foreground program died without cleanup (e.g. a
    // force-closed SSH session whose remote TUI never sent DECRST) —
    // inject the reset so client Terms, this Term, and future replay
    // snapshots all clear together.
    clear_stale_mouse_modes_at_prompt(state).await;

    // Step 2c: runs after the stale-mode reset so a 1004 the reset just
    // cleared is not treated as newly enabled.
    deliver_focus_state_when_reporting_enables(state).await;

    // Steps 3–5: Metadata uses original bytes (OSC parser doesn't care about CSI ED 3).
    process_metadata_events(state).await;
}

/// Answer the PTY and fan committed image records out for one read.
///
/// Ordering is the whole point: every reply this read owes is written to the
/// PTY in image-output-sequence order first, then the canonical mutations are
/// committed and published to the capable sinks. Both steps are skipped for a
/// session whose capability is unlatched or killed, so a disabled Scribe
/// answers no discovery probe and leaks no records.
async fn deliver_image_commit(state: &mut PtyReaderState, commit: &SessionTerminalCommit) {
    let (enabled, required) = {
        let sharing = lock_image_sharing(&state.image_sharing);
        (sharing.images_enabled(), sharing.effective())
    };
    let mut replies = 0u64;
    for reply in plan_pty_replies(commit, enabled) {
        replies += 1;
        debug!(
            session_id = %state.session_id,
            sequence = reply.sequence.0,
            bytes = reply.bytes.len(),
            "writing terminal image reply to PTY"
        );
        write_term_response(&state.pty_write, state.session_id, &reply.bytes).await;
    }
    if !enabled {
        return;
    }
    // The seam backs each definition with the canonical RGBA it retained while
    // committing this read, so the provider adds nothing here. A definition the
    // session could not pay to retain is withdrawn together with every
    // placement naming it, and the viewer converges on a scene holding exactly
    // what the server can prove.
    let images_handle = Arc::clone(&state.terminal_images);
    let mut images = images_handle.lock().await;
    let messages = match images.commit_and_publish(commit, &mut |_| None) {
        Ok(messages) => messages,
        Err(error) => {
            warn!(
                session_id = %state.session_id,
                error = %error,
                "terminal image publication rejected a committed read"
            );
            return;
        }
    };
    record_image_application_evidence(state, &images, commit, replies);
    let session_id = state.session_id;
    if !messages.is_empty() {
        let frames: Vec<ServerMessage> = messages
            .into_iter()
            .map(|message| ServerMessage::TerminalImageLive { session_id, message })
            .collect();
        let viewers = send_image_records(&state.client_writer, session_id, required, &frames);
        debug!(
            %session_id,
            records = frames.len(),
            viewers,
            "fanned terminal image records to capable viewers"
        );
    }
    deliver_image_replay(&state.client_writer, state.session_id, &images, required);
}

/// Pay a just-attached sink's replay debt without waiting for a PTY read.
///
/// The commit path drains debt on the session's next committed read, which on a
/// quiet pane never comes: a viewer attaching to an idle image session would
/// paint text with no images until the application happened to write a byte.
/// The attach path calls this once its sink is live, so the two ways a sink
/// accrues debt each have a drain that fires when they happen.
// @lat: [[terminal-images#Terminal Images#Combined Image Replay#Replay debt]]
pub async fn drain_image_replay_debt(
    client_writer: &ClientWriter,
    session_id: SessionId,
    images: &SessionImageState,
    sharing: &SharedImageSharing,
) {
    let (enabled, required) = {
        let sharing = lock_image_sharing(sharing);
        (sharing.images_enabled(), sharing.effective())
    };
    // An unlatched or disabled session has no scene to owe, so the common
    // image-free attach never touches the image lock at all.
    if !enabled {
        return;
    }
    let images = images.lock().await;
    deliver_image_replay(client_writer, session_id, &images, required);
}

/// Give every capable sink that owes one the session's whole canonical scene.
///
/// A sink owes a replay when it just attached or when its queued output was
/// shed — the two ways a viewer's scene stops being knowable. One burst is
/// planned from canonical state and fanned to all of them, so the recovery cost
/// is independent of viewer count and the server never keeps a per-sink copy.
// @lat: [[terminal-images#Terminal Images#Combined Image Replay]]
fn deliver_image_replay(
    client_writer: &ClientWriter,
    session_id: SessionId,
    images: &PtyTerminalImageState,
    required: TerminalImageCapabilities,
) {
    let debt = image_replay_debt(client_writer, required);
    if debt == 0 {
        return;
    }
    let snapshot = images.state();
    let definitions = images.canonical_definitions();
    let placements = images.canonical_placements();
    let plan = terminal_image_replay::plan_replay(
        &terminal_image_replay::ReplayInputs {
            generation: snapshot.generation,
            through_sequence: snapshot.sequence,
            active_screen: snapshot.active_screen,
            definitions: &definitions,
            placements: &placements,
        },
        &mut |definition| images.canonical_rgba(definition),
    );
    let records: Vec<ServerMessage> = plan
        .records
        .into_iter()
        .map(|message| ServerMessage::TerminalImageReplay { session_id, message })
        .collect();
    let viewers = send_image_replay(client_writer, required, &records);
    debug!(
        %session_id,
        generation = snapshot.generation.0,
        through_sequence = snapshot.sequence.0,
        records = records.len(),
        chunks = plan.counters.chunks,
        rgba_bytes = plan.counters.total_rgba_bytes,
        withdrawn_definitions = plan.counters.withdrawn_definitions,
        debt,
        viewers,
        "replayed the canonical terminal image scene to capable viewers"
    );
}

/// Write one line naming what a real application's graphics did, when it
/// changes.
///
/// A live viewer does not receive canonical pixels yet, so this summary is the
/// only place a pinned application's protocol choice becomes observable
/// end-to-end: which protocol it transmitted, whether Scribe answered its
/// discovery probe, and which placement kind survived on the grid.
// @lat: [[terminal-images#Terminal Images#Pinned Application Corpus]]
fn record_image_application_evidence(
    state: &mut PtyReaderState,
    images: &PtyTerminalImageState,
    commit: &SessionTerminalCommit,
    replies: u64,
) {
    let mut evidence = state.image_evidence;
    evidence.replies += replies;
    for output in commit.outputs.as_slice() {
        let SessionTerminalOutput::Image { boundary, .. } = output else { continue };
        match boundary {
            TerminalImageBoundary::Kitty { decoded, .. } => {
                evidence.kitty_commands += 1;
                evidence.kitty_transfers += u64::from(decoded.is_some());
            }
            TerminalImageBoundary::Sixel { .. } => evidence.sixel_images += 1,
            TerminalImageBoundary::Failure(failure) => {
                evidence.failures += 1;
                debug!(
                    session_id = %state.session_id,
                    protocol = ?failure.protocol,
                    category = ?failure.category,
                    limit = ?failure.limit,
                    "terminal image application output failed"
                );
            }
            TerminalImageBoundary::SixelMode { .. } => {}
        }
    }
    evidence.classic_placements = 0;
    evidence.placeholder_placements = 0;
    evidence.sixel_placements = 0;
    for (_, placement) in images.canonical_placements() {
        match placement.kind {
            TerminalImagePlacementKind::KittyClassic => evidence.classic_placements += 1,
            TerminalImagePlacementKind::KittyUnicodePlaceholder => {
                evidence.placeholder_placements += 1;
            }
            TerminalImagePlacementKind::Sixel => evidence.sixel_placements += 1,
        }
    }
    if evidence == state.image_evidence {
        return;
    }
    state.image_evidence = evidence;
    debug!(
        session_id = %state.session_id,
        replies = evidence.replies,
        kitty_commands = evidence.kitty_commands,
        kitty_transfers = evidence.kitty_transfers,
        sixel_images = evidence.sixel_images,
        failures = evidence.failures,
        classic_placements = evidence.classic_placements,
        placeholder_placements = evidence.placeholder_placements,
        sixel_placements = evidence.sixel_placements,
        "terminal image application evidence"
    );
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
    let commit = feed_term(
        &state.term,
        &state.term_commit,
        &state.search_cache,
        &mut state.ansi_processor,
        &resets,
    )
    .await;
    send_pty_output(&state.client_writer, state.session_id, &resets, commit);
}

/// Deliver the session's current focus state when the application newly
/// enables focus reporting (DECSET 1004).
///
/// Focus events are relayed only to sessions with the mode active, so an
/// application that enables it *after* the client's last focus report — an AI
/// CLI doing so during startup in the already-focused pane — would otherwise
/// never learn it holds focus, and one that gates its own cursor on focus-in
/// (Claude Code) draws none until some unrelated focus change. Reporting the
/// state at enable time mirrors tmux.
async fn deliver_focus_state_when_reporting_enables(state: &mut PtyReaderState) {
    let active = {
        let term_guard = state.term.lock().await;
        term_guard.mode().contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT)
    };
    if !focus_mode_newly_enabled(&mut state.focus_mode_was_active, active) {
        return;
    }
    let focused = state.has_focus.load(Ordering::Relaxed);
    let event = if focused { FOCUS_GAINED } else { FOCUS_LOST };
    debug!(
        session_id = %state.session_id,
        focused,
        "focus reporting enabled; delivering current focus state"
    );
    let mut pty_write = state.pty_write.lock().await;
    if let Err(e) = pty_write.write_all(event).await {
        debug!("initial focus event write failed: {e}");
    }
}

const FOCUS_GAINED: &[u8] = b"\x1b[I";
const FOCUS_LOST: &[u8] = b"\x1b[O";

/// Latch `active` into `was_active`, reporting whether this chunk turned
/// focus reporting on — the off→on edge that owes the PTY the current focus
/// state. Every other transition (steady states, the disable edge) owes
/// nothing: those are either already covered by `handle_focus_changed` or
/// meaningless to an application that just turned reporting off.
fn focus_mode_newly_enabled(was_active: &mut bool, active: bool) -> bool {
    let newly_enabled = active && !*was_active;
    *was_active = active;
    newly_enabled
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

/// Trim this Term's duplicate AI-redraw history, returning the kept row count
/// and the commit-cursor value the trim advanced to (so the matching
/// `TrimScrollback` frame can be tagged with it).
async fn handle_suppressed_ai_ed3(state: &mut PtyReaderState) -> Option<(usize, u64)> {
    let current_history = {
        let term_guard = state.term.lock().await;
        term_guard.grid().history_size()
    };
    let kept_rows = state.preserved_ai_scrollback.trim_target(current_history)?;
    let commit = trim_term_scrollback(
        &state.term,
        &state.term_commit,
        kept_rows,
        state.scrollback_lines.load(Ordering::Relaxed),
    )
    .await;
    Some((kept_rows, commit))
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

/// Trim the Term's scrollback and advance the commit cursor past the trim,
/// returning its new value.
///
/// A scrollback trim carries no bytes, so it ticks the cursor by one: an attach
/// snapshot taken before the trim sits below that value and therefore still
/// flushes the client's `TrimScrollback`, while one taken after already contains
/// the trim and drops it.
async fn trim_term_scrollback(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: &TermCommit,
    kept_rows: usize,
    max_rows: usize,
) -> u64 {
    let mut term_guard = term.lock().await;
    trim_term_scrollback_inner(&mut term_guard, kept_rows, max_rows);
    term_commit.advance(1)
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

/// Route the reader's own exit into the session funnel (spec 017 US1-3).
///
/// The end of the master stream only proves that every slave fd closed, which
/// a live child can do on its own, so when a child-exit watcher is armed the
/// reader stops reading and leaves emission to it. Handoff-inherited sessions
/// never arm a watcher and keep that path with `exit_code: None`. Cancellation
/// still calls the funnel: the canceller normally wins the CAS, but the reader
/// is the backstop if it is torn down before the canceller gets there.
async fn finalize_pty_reader(state: PtyReaderState, stop: ReaderStop) {
    info!(session_id = %state.session_id, ?stop, "PTY reader task exited");
    // Release the watcher's drain wait before anything else, whichever way the
    // loop ended: it is parked on this until the PTY stops producing.
    state.exit_gate.mark_reader_done();
    if stop.ends_master_stream() && state.exit_gate.has_watcher() {
        debug!(
            session_id = %state.session_id,
            "PTY master stream ended with a child-exit watcher armed — deferring session exit"
        );
        return;
    }
    finalize_session_exit(
        &state.exit_gate,
        SessionExitContext {
            session_id: state.session_id,
            client_writer: &state.client_writer,
            attachment: &state.attachment,
            live_sessions: &state.live_sessions,
            window_shares: &state.window_shares,
            workspace_manager: &state.workspace_manager,
        },
        ChildExit::UNKNOWN,
    )
    .await;
}

/// Everything the one-shot session finalizer touches, assembled by whichever
/// exit path reaches the funnel. Close paths clone these off the `LiveSession`
/// via [`LiveSession::exit_handles`] before dropping it; the reader owns them
/// directly.
struct SessionExitContext<'a> {
    session_id: SessionId,
    client_writer: &'a ClientWriter,
    attachment: &'a SessionAttachment,
    live_sessions: &'a LiveSessionRegistry,
    window_shares: &'a WindowShares,
    workspace_manager: &'a Arc<RwLock<WorkspaceManager>>,
}

/// The single idempotent session-exit finalizer (spec 017 US1-3).
///
/// Reader EOF, reader cancellation, an explicit `CloseSession`/`CloseWindow`,
/// and the child-exit watcher all end here. The CAS in
/// [`SessionExitGate::claim_exit`] elects exactly one of them to publish
/// `SessionExited` and unwire the session, so a close racing the child's own
/// death can neither double-emit nor drop the notification. `exit` is
/// [`ChildExit::UNKNOWN`] for every path except the watcher, which is the only
/// observer of a real wait status.
///
/// Winning the CAS also cancels the reader, because the winner is not always a
/// path that already did (spec 017 US1-3). Both close handlers cancel before
/// they get here, but the child-exit watcher does not, and its session's
/// reader can still be parked on a master that will never end — a descendant
/// that inherited the slave fd keeps it open long after the child is gone.
/// Left running, that reader outlives the registry entry it belongs to,
/// feeding a `Term` and sinks nobody can reach. The watcher has already waited
/// out its drain grace by the time it arrives, so nothing still deliverable is
/// cut short.
///
/// This runs on the reader task too, so it never joins the reader; it takes
/// the two write guards in the documented order (`live_sessions`, then
/// `workspace_manager`) and holds neither across an `.await`. That is exactly
/// why [`join_reader_bounded`] must be called with no guard held.
async fn finalize_session_exit(
    gate: &SessionExitGate,
    ctx: SessionExitContext<'_>,
    exit: ChildExit,
) {
    if !gate.claim_exit() {
        debug!(session_id = %ctx.session_id, "session exit already finalized");
        return;
    }
    gate.cancel();
    // The reader/watch path owns the only session-end route where the entry is
    // still live here. Clear first so the owner cannot retain a halo after the
    // terminal that produced it is gone.
    set_focused_issue(ctx.session_id, None, ctx.live_sessions, ctx.window_shares).await;
    let exit_msg = ServerMessage::SessionExited {
        session_id: ctx.session_id,
        exit_code: exit.exit_code,
        signal: exit.signal,
    };
    send_to_client(ctx.client_writer, None, &exit_msg);
    remove_from_session_attachment(ctx.attachment, ctx.session_id).await;
    // Bind the removed session: dropping it inline would run `Pty::Drop`
    // (SIGHUP + blocking `waitpid`) while the registry write guard is still
    // held. Teardown happens after the guard is gone, on the blocking pool.
    let mut removed = ctx.live_sessions.write().await.remove(&ctx.session_id);
    if let Some(pty) = removed.as_mut().and_then(LiveSession::take_pty) {
        pty.teardown();
    }
    drop(removed);
    let mut workspace_manager = ctx.workspace_manager.write().await;
    workspace_manager.remove_session(ctx.session_id);
    workspace_manager.remove_session_from_window(ctx.session_id);
    info!(session_id = %ctx.session_id, ?exit, "session exit finalized");
}

/// Result of a PTY read attempt, or of the cancellation racing it.
enum ReadResult {
    Data(usize),
    Eof,
    /// The session's exit gate was cancelled while the read was parked.
    Cancelled,
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

/// Send raw PTY output to the client (fast path). No-op when detached, and a
/// no-op for an empty chunk. `commit` is the [`TermCommit`] value this chunk
/// reaches once the Term has consumed it.
///
/// [`apply_pty_filters`] can consume a whole read — a chunk that is exactly
/// `\x1b[3J`, or one held entirely in a filter's partial-match state — and a
/// zero-byte frame would still be allocated, serialized, queued, coalesced,
/// and parsed by every attached client for no visible effect. Dropping it here
/// covers every producer, and costs the client nothing: an empty chunk leaves
/// the commit cursor where it was, while an accompanying `TrimScrollback`
/// frame carries the cursor value the attach path needs.
fn send_pty_output(client_writer: &ClientWriter, session_id: SessionId, bytes: &[u8], commit: u64) {
    if bytes.is_empty() {
        return;
    }
    let msg = ServerMessage::PtyOutput { session_id, data: bytes.to_vec() };
    send_to_client(client_writer, Some(commit), &msg);
}

/// Tell clients to trim their mirrored scrollback to `history_rows`. `commit` is
/// the [`TermCommit`] value the matching Term-side trim advanced the cursor to.
fn send_trim_scrollback(
    client_writer: &ClientWriter,
    session_id: SessionId,
    history_rows: usize,
    commit: u64,
) {
    let msg = ServerMessage::TrimScrollback {
        session_id,
        history_rows: u32::try_from(history_rows).unwrap_or(u32::MAX),
    };
    send_to_client(client_writer, Some(commit), &msg);
}

/// Feed bytes into the terminal emulator via the ANSI processor and advance the
/// session's commit cursor past them, returning its new value.
///
/// Both happen under one `Term` lock so "the cursor reads C" and "the Term has
/// consumed everything tagged ≤ C" can never disagree — the invariant the attach
/// snapshot relies on to decide which buffered frames it already contains. The
/// guard is dropped before returning, i.e. before any subsequent `.await`.
///
/// The find overlay's cached scrollback snapshot is dropped in the same critical
/// section (spec 017 US8-2): these bytes are exactly the "new session output"
/// that invalidates it, and releasing it here frees its allocation immediately
/// instead of at whatever later keystroke would have noticed.
async fn feed_term(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: &TermCommit,
    search_cache: &SearchSnapshotCache,
    ansi_processor: &mut AnsiProcessor,
    bytes: &[u8],
) -> u64 {
    let mut term_guard = term.lock().await;
    ansi_processor.advance(&mut *term_guard, bytes);
    search_cache.invalidate();
    term_commit.advance(chunk_len(bytes))
}

/// One owned image result and its exact source bytes for the real Term feed.
struct ObservedImageResultFeed<'a> {
    observer: TerminalGridObserverHandle,
    bytes: &'a [u8],
    image_result: Result<SessionTerminalCommit, SessionTerminalError>,
}

async fn feed_term_image_result_observed(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    term_commit: &TermCommit,
    search_cache: &SearchSnapshotCache,
    ansi_processor: &mut AnsiProcessor,
    feed: ObservedImageResultFeed<'_>,
) -> (Result<SessionTerminalCommit, SessionTerminalError>, Option<ObservedTerminalGridSpan>) {
    feed_terminal_image_result_production(
        ProductionTerminalFeed::new(&feed.observer, term, ansi_processor),
        feed.bytes,
        feed.image_result,
        || {
            search_cache.invalidate();
            term_commit.advance(chunk_len(feed.bytes));
        },
    )
    .await
}

/// Flush a synchronized update after its timeout elapses.
async fn stop_term_sync(
    term: &Arc<Mutex<alacritty_terminal::Term<scribe_pty::event_listener::ScribeEventListener>>>,
    observer: &TerminalGridObserverHandle,
    ansi_processor: &mut AnsiProcessor,
) -> TerminalGridObservation {
    flush_terminal_observed_production(observer, term, ansi_processor).await.observation
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
        handle_clipboard_command(state, cmd).await;
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
                MetadataRuntime {
                    workspace_manager: &state.workspace_manager,
                    live_sessions: &state.live_sessions,
                    window_shares: &state.window_shares,
                    git_ref_watcher: &state.git_ref_watcher,
                },
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
            // The authoritative Term answers DA1 for this session; Sixel
            // discovery is attribute 4 appended to that same reply, exactly
            // once and only while the capability is actually live.
            let sixel_enabled = lock_image_sharing(&state.image_sharing).images_enabled();
            let response = augment_device_attributes(&text, sixel_enabled);
            write_term_response(&state.pty_write, state.session_id, response.as_bytes()).await;
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
    state
        .window_shares
        .read()
        .await
        .get(&state.window_id)
        .is_some_and(WindowShare::clipboard_gating)
}

/// Whether *any* client writer is currently attached to this session. Used
/// in tandem with `client_clipboard_gating` to gate OSC 52 prompt and bridge
/// dispatch — both checks must succeed before a `ServerMessage::Clipboard*`
/// variant goes on the wire.
fn session_has_attached_client(state: &PtyReaderState) -> bool {
    !lock_sinks(&state.client_writer).is_empty()
}

/// Feature 015 (T030, D7/FR-013): route a session-initiated OSC 52 request to the
/// single control-holder sink, falling back to the owning machine when control is
/// unheld or the mode is free-for-all — this is exactly `controller_participant`'s
/// holder→owner precedence, and it is the same participant whose `clipboard_gating`
/// bit [`client_clipboard_gating`] already checked. A default-safe route: an
/// unattended viewer never gets a surprise clipboard prompt. Sends to ONE sink, not
/// the participant fan-out; a no-op when the window has no share.
async fn send_clipboard_to_target(state: &PtyReaderState, msg: &ServerMessage) {
    let target = state
        .window_shares
        .read()
        .await
        .get(&state.window_id)
        .and_then(|share| share.controller_writer().cloned());
    if let Some(writer) = target {
        send_message(&writer, msg).await;
    }
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

    let headless = !session_has_attached_client(state) || !client_clipboard_gating(state).await;

    match policy.write_mode {
        ClipboardMode::Deny => {}
        ClipboardMode::Allow => {
            if headless {
                return;
            }
            send_clipboard_to_target(
                state,
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
    send_clipboard_to_target(
        state,
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
    let headless = !session_has_attached_client(state) || !client_clipboard_gating(state).await;

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
            send_clipboard_to_target(
                state,
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
    send_clipboard_to_target(
        state,
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
    send_clipboard_to_target(
        state,
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
        send_clipboard_to_target(
            state,
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

/// Resolve a palette index the live `Term` has no override for against the
/// configured theme.
///
/// Reads the process-wide snapshot rather than `load_config` + `resolve_theme`
/// so a 256-index OSC 4 probe costs zero disk reads once warm; the snapshot is
/// dropped by [`handle_config_reloaded`] whenever the file changes.
fn fallback_term_color(index: usize) -> Option<alacritty_terminal::vte::ansi::Rgb> {
    let snapshot = scribe_config::config_snapshot().ok()?;

    theme_color_for_index(&snapshot.theme, index).map(theme_color_to_rgb)
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
        MetadataEvent::TitleChanged(_) | MetadataEvent::IconTitleChanged(_) => *saw_title = true,
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
        MetadataRuntime {
            workspace_manager: &state.workspace_manager,
            live_sessions: &state.live_sessions,
            window_shares: &state.window_shares,
            git_ref_watcher: &state.git_ref_watcher,
        },
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
        MetadataRuntime {
            workspace_manager: &state.workspace_manager,
            live_sessions: &state.live_sessions,
            window_shares: &state.window_shares,
            git_ref_watcher: &state.git_ref_watcher,
        },
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

/// How long a resolved branch stays usable for a directory the session is
/// still sitting in. A `ListSessions` read can arrive at any time and has no
/// invalidation signal of its own, so the TTL bounds how stale a branch
/// switched from outside Scribe (`git checkout` in another terminal) can be.
const GIT_BRANCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// A branch resolution and the directory it was taken in.
struct CachedGitBranch {
    cwd: std::path::PathBuf,
    branch: Option<String>,
    resolved_at: std::time::Instant,
}

/// Per-session memo of the last [`detect_git_branch`] walk, keyed on the
/// directory it was taken in — a `(session, cwd)` key, since the cache lives
/// on the session.
///
/// A CWD report that survives [`record_cwd_report`] changes the key and is
/// therefore the invalidation signal; [`GIT_BRANCH_CACHE_TTL`] bounds staleness
/// for a session that never moves. Interior mutability so a resolved branch can
/// be stored back while holding only a read guard on the live-session registry;
/// no caller walks `.git/HEAD` with a registry guard held.
#[derive(Default)]
struct GitBranchCache(std::sync::Mutex<Option<CachedGitBranch>>);

/// Outcome of a [`GitBranchCache`] lookup. `Hit(None)` — "cached: this
/// directory is not in a repository" — is a real answer and must stay
/// distinguishable from a miss.
enum BranchLookup {
    Hit(Option<String>),
    Miss,
}

impl GitBranchCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<CachedGitBranch>> {
        // The critical section is a path compare and a clone, so poisoning
        // cannot occur in practice; recover rather than propagate if it does.
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The cached branch for `cwd`, or a miss when the entry names another
    /// directory, has expired, or was never taken.
    fn fresh(&self, cwd: &Path) -> BranchLookup {
        let entry = self.lock();
        match entry.as_ref() {
            Some(entry)
                if entry.cwd == cwd && entry.resolved_at.elapsed() < GIT_BRANCH_CACHE_TTL =>
            {
                BranchLookup::Hit(entry.branch.clone())
            }
            _ => BranchLookup::Miss,
        }
    }

    fn store(&self, cwd: &Path, branch: Option<String>) {
        *self.lock() = Some(CachedGitBranch {
            cwd: cwd.to_path_buf(),
            branch,
            resolved_at: std::time::Instant::now(),
        });
    }
}

/// Resolve a session's branch through its [`GitBranchCache`] without holding
/// the registry guard across the filesystem walk.
async fn resolve_session_git_branch(
    session_id: SessionId,
    cwd: &Path,
    live_sessions: &LiveSessionRegistry,
) -> Option<String> {
    let lookup = live_sessions
        .read()
        .await
        .get(&session_id)
        .map_or(BranchLookup::Miss, |s| s.git_branch_cache.fresh(cwd));
    if let BranchLookup::Hit(branch) = lookup {
        return branch;
    }
    let branch = detect_git_branch(cwd);
    if let Some(session) = live_sessions.read().await.get(&session_id) {
        session.git_branch_cache.store(cwd, branch.clone());
    }
    branch
}

/// Resolve the branch of every session whose [`GitBranchCache`] missed during
/// a `SessionList` build, off the registry and workspace-manager guards the
/// caller has already released.
///
/// Panes are independent sessions with independent memos, so a split window
/// sitting in one repository misses once per pane. The walk is therefore keyed
/// on the directory rather than the session: distinct directories cost one
/// `.git/HEAD` walk each, however many panes share them. Every pending session
/// is then fed its answer so the next read is a hit.
async fn resolve_pending_git_branches(
    pending: &[(SessionId, std::path::PathBuf)],
    live_sessions: &LiveSessionRegistry,
) -> HashMap<SessionId, Option<String>> {
    let mut by_cwd: HashMap<&Path, Option<String>> = HashMap::new();
    for (_, cwd) in pending {
        by_cwd.entry(cwd.as_path()).or_insert_with(|| detect_git_branch(cwd));
    }

    let sessions = live_sessions.read().await;
    let mut resolved = HashMap::with_capacity(pending.len());
    for (session_id, cwd) in pending {
        let branch = by_cwd.get(cwd.as_path()).cloned().flatten();
        if let Some(session) = sessions.get(session_id) {
            session.git_branch_cache.store(cwd, branch.clone());
        }
        resolved.insert(*session_id, branch);
    }
    resolved
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
/// state of its own. Four drop conditions, all silent:
///
/// - The session has no live `ai_state` yet — nothing to patch. The first
///   real state event (Stop hook, `PreToolUse`, etc.) will establish state;
///   the next refresh will then take effect.
/// - The live state's provider differs from the refresh's provider. Cross-
///   provider context bleed (e.g. Codex's `CodexContext=NN` arriving while
///   the live state is Claude's) is rejected as a defensive guard.
/// - The session is gone (closed during the await race).
/// - The refresh repeats the percentage already on the live state. Status
///   lines poll on a timer and re-report the same fill for as long as the
///   context does not move, so the patch is a no-op and the frame would tell
///   every attached client nothing it does not already hold.
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
        if ai_state.context == Some(context) {
            return;
        }
        ai_state.context = Some(context);
        ai_state.clone()
    };
    let server_msg = ServerMessage::AiStateChanged { session_id, ai_state: updated_state };
    send_to_client(client_writer, None, &server_msg);
}

/// Persist metadata from a `ServerMessage` into the live session registry.
async fn persist_session_metadata(
    server_msg: &ServerMessage,
    session_id: SessionId,
    live_sessions: &LiveSessionRegistry,
) {
    match server_msg {
        ServerMessage::TitleChanged { title, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.title = (!title.trim().is_empty()).then(|| title.clone());
            })
            .await;
        }
        ServerMessage::IconTitleChanged { title, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.icon_title = (!title.trim().is_empty()).then(|| title.clone());
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
            let at = std::time::SystemTime::now();
            update_live_session(session_id, live_sessions, |session| {
                retain_ai_state(session, ai_state, at);
            })
            .await;
        }
        ServerMessage::AiStateCleared { .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session.ai_state = None;
                session.ai_provider_hint = None;
                session.task_label = None;
                // Same boundary the client's `AiChrome::forget` uses: the
                // provider exiting must take the prompt bar with it, or the
                // next client to attach paints a bar for a dead conversation.
                session.prompt_state = None;
            })
            .await;
        }
        ServerMessage::PromptReceived { text, .. } => {
            update_live_session(session_id, live_sessions, |session| {
                session
                    .prompt_state
                    .get_or_insert_with(Default::default)
                    .record_prompt(text, std::time::SystemTime::now());
            })
            .await;
        }
        _ => {}
    }
}

/// Fold one `AiStateChanged` onto the chrome a session retains for the next
/// client to attach.
///
/// A state edge naming a *different* conversation retires the previous one's
/// prompt history, the same edge the client's `AiChrome::note_conversation`
/// folds. Without the server half, the next `SessionList` — or a snapshot
/// written after the switch — repaints the retired conversation's rows and
/// resumes its count from where the dead conversation left off.
fn retain_ai_state(
    session: &mut LiveSession,
    ai_state: &scribe_common::ai_state::AiProcessState,
    at: std::time::SystemTime,
) {
    if session.ai_state.as_ref().is_some_and(|prev| ai_state.switched_conversation_from(prev)) {
        session.prompt_state = None;
    }
    if let Some(prompts) = session.prompt_state.as_mut() {
        prompts.note_prompt_progress(&ai_state.state, at);
    }
    session.ai_state = Some(ai_state.clone());
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

/// Record a CWD report and answer whether it differs from the previous one.
///
/// Shells emit OSC 7 from their prompt hook, so a session that never leaves
/// its directory still reports the same path on every command. Everything
/// downstream of this check — the registry write, the `CwdChanged` and
/// `GitBranch` frames, the `.git/HEAD` walk, the workspace-manager write
/// lock — is waste for such a repeat, and a report that does get through is
/// the invalidation signal for anything caching per-CWD state.
///
/// A session missing from the registry has no previous value to compare
/// against, so it is reported as changed and the caller behaves as before.
async fn record_cwd_report(
    session_id: SessionId,
    cwd: &Path,
    live_sessions: &LiveSessionRegistry,
) -> bool {
    let mut sessions = live_sessions.write().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return true;
    };
    if session.last_cwd_report.as_deref() == Some(cwd) {
        return false;
    }
    session.last_cwd_report = Some(cwd.to_path_buf());
    true
}

/// Hand an observed session CWD to Git without blocking a Tokio worker.
fn observe_git_repository(git_ref_watcher: &Arc<GitRefWatcherControl>, cwd: &Path) {
    if !git_ref_watcher.is_running() {
        return;
    }
    let git_ref_watcher = Arc::clone(git_ref_watcher);
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = git_ref_watcher.watch_cwd(&cwd) {
            debug!(%error, cwd = %cwd.display(), "Git repository watch skipped");
        }
    });
}

/// Convert a `MetadataEvent` to a `ServerMessage` and send it.
/// For `CwdChanged`, also notifies the workspace manager and sends git branch.
/// Workspace naming always runs (even when detached) so names are ready on
/// reconnect. Client messages are only sent when attached.
///
/// A `CwdChanged` repeating the session's last reported directory is dropped
/// by [`record_cwd_report`] before any of that work happens.
///
/// Exposed publicly so `hook_ingress` can feed the same pipeline.
pub async fn send_metadata_event(
    event: MetadataEvent,
    session_id: SessionId,
    client_writer: &ClientWriter,
    runtime: MetadataRuntime<'_>,
) {
    let MetadataRuntime { workspace_manager, live_sessions, window_shares, git_ref_watcher } =
        runtime;
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
    // brings the client view in line with the server's interpretation. Prompt
    // history and the provider hint count too: a keystroke may have dismissed
    // the visible attention state just before the shell prompt returns.
    let synthesize_ai_cleared =
        matches!(&event, MetadataEvent::PromptMark { kind: PromptMarkKind::PromptStart, .. })
            && live_sessions.read().await.get(&session_id).is_some_and(|s| {
                s.ai_state.is_some() || s.ai_provider_hint.is_some() || s.prompt_state.is_some()
            });

    let Some((mut server_msg, cwd_for_workspace)) = convert_metadata_event(event, session_id)
    else {
        return;
    };

    // Drop a per-prompt OSC 7 that names the directory the session is
    // already in, ahead of every consumer of the report.
    if let Some(cwd) = cwd_for_workspace.as_deref()
        && !record_cwd_report(session_id, cwd, live_sessions).await
    {
        return;
    }

    merge_partial_ai_state(&mut server_msg, session_id, live_sessions).await;

    let clears_focused_issue = matches!(&server_msg, ServerMessage::AiStateCleared { .. });
    persist_session_metadata(&server_msg, session_id, live_sessions).await;

    send_to_client(client_writer, None, &server_msg);
    if clears_focused_issue {
        set_focused_issue(session_id, None, live_sessions, window_shares).await;
    }

    if synthesize_ai_cleared {
        let clear_msg = ServerMessage::AiStateCleared { session_id };
        persist_session_metadata(&clear_msg, session_id, live_sessions).await;
        send_to_client(client_writer, None, &clear_msg);
        set_focused_issue(session_id, None, live_sessions, window_shares).await;
    }

    if let Some(cwd) = cwd_for_workspace {
        observe_git_repository(git_ref_watcher, &cwd);
        // Send git branch information for the new CWD. A detached session has
        // no sink to render the branch, so the walk is pure waste — the
        // `SessionList` reply that precedes the next attach resolves it.
        let attached = !lock_sinks(client_writer).is_empty();
        if attached {
            let branch = resolve_session_git_branch(session_id, &cwd, live_sessions).await;
            let git_msg = ServerMessage::GitBranch { session_id, branch };
            send_to_client(client_writer, None, &git_msg);
        }

        // Always update workspace naming, even when detached.
        let named_msg = {
            let mut wm = workspace_manager.write().await;
            wm.on_cwd_changed(session_id, &cwd)
        };
        if let Some(msg) = named_msg {
            send_to_client(client_writer, None, &msg);
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
        MetadataEvent::IconTitleChanged(title) => {
            Some((ServerMessage::IconTitleChanged { session_id, title }, None))
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

/// Create a new empty `WindowShares` registry (feature 015 T006). Replaces the
/// pre-015 `new_connected_clients` / `new_window_clipboard_gating` /
/// `new_window_controllers` constructors.
pub fn new_window_shares() -> WindowShares {
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
    // One export per payload, so the shared image-byte ceiling is charged
    // across every session rather than per session. With the master switch off
    // nothing is exported and the payload stays v6, which is what makes a
    // rollback to a pre-image server a config change instead of a cold restart.
    let mut images =
        crate::terminal_image_handoff::HandoffImageExport::new(images_master_enabled());

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

        // Reads are already paused, so the seam is quiescent and this lock is
        // uncontended: whatever the last read committed is the whole scene.
        let seam = live.terminal_images.lock().await;
        let image_state =
            images.session(seam.session(), &mut |definition| seam.canonical_rgba(definition));
        drop(seam);

        handoff_sessions.push(HandoffSession {
            session_id,
            workspace_id: live.workspace_id,
            child_pid: live.child_pid,
            child_identity: live.child_identity,
            cols,
            rows,
            cell_width: live.cell_width,
            cell_height: live.cell_height,
            snapshot: None,
            session_replay,
            title: live.title.clone(),
            icon_title: live.icon_title.clone(),
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
            shell_tool: live.shell_tool,
            prompt_state: live.prompt_state.clone(),
            env_window_id: Some(live.env_window_id),
            env_envelope_id: live.env_envelope_id.clone(),
            image_state,
        });

        fds.push(Arc::clone(&live.resize_fd));
    }

    let counters = images.counters();
    if counters.sessions > 0 {
        info!(
            sessions = counters.sessions,
            definitions = counters.definitions,
            placements = counters.placements,
            chunks = counters.chunks,
            rgba_bytes = counters.total_rgba_bytes,
            dropped_scenes = counters.dropped_scenes,
            partial_framers = counters.partial_framers,
            pending_transfers = counters.pending_transfers,
            "exported terminal image state for handoff"
        );
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
    window_shares: &WindowShares,
    git_ref_watcher: &Arc<GitRefWatcherControl>,
) {
    let pending = session_manager.pending_session_ids().await;

    for (session_id, workspace_id) in pending {
        if let Some(session) = session_manager.take_session(session_id).await {
            // Look up the handoff-restored membership owner. A preserved
            // `env_window_id` on the session remains authoritative for env
            // cleanup; this value is the backward-compatible fallback for
            // payloads that predate those handoff fields.
            let window_id = workspace_manager
                .read()
                .await
                .window_for_session(session_id)
                .unwrap_or_else(WindowId::new);
            start_session(
                StartSessionIds { session: session_id, workspace: workspace_id, window: window_id },
                session,
                InitialAttachment { writer: None, attached_ids: None },
                SessionRuntimeContext {
                    workspace_manager,
                    live_sessions,
                    git_ref_watcher,
                    window_shares,
                },
            )
            .await;
            info!(%session_id, "activated restored session (detached)");
        }
    }
}

/// Stop every PTY reader before the process exits (spec 017 US1-3).
///
/// Shutdown was otherwise the one exit path that abandoned tasks parked on a
/// PTY read, so it runs the same cancel-then-bounded-join the close handlers
/// use. It publishes nothing: each gate's exit CAS is claimed up front, so the
/// funnel a cancelled reader reaches finds it taken and returns without
/// emitting `SessionExited` or re-entering the registry. There is no socket
/// left to notify by then, and the children stay with the `PtyGuard`s still
/// parked on their sessions, whose `Drop` hangs them up off-worker as the
/// registry unwinds — the pre-existing shutdown behaviour, unchanged.
///
/// Never call this on the handoff path; see [`defuse_for_handoff`].
pub async fn shutdown_pty_readers(live_sessions: &LiveSessionRegistry) {
    // Snapshot under a read guard and drop it before joining: a reader that
    // claimed the funnel first is still entitled to the registry write lock on
    // its way out, and the join must not be holding anything when it takes it.
    let gates: Vec<(SessionId, Arc<SessionExitGate>)> = {
        let sessions = live_sessions.read().await;
        sessions.iter().map(|(&sid, session)| (sid, Arc::clone(&session.exit_gate))).collect()
    };
    if gates.is_empty() {
        return;
    }
    info!(count = gates.len(), "stopping PTY readers for shutdown");
    for (_, gate) in &gates {
        let _claimed = gate.claim_exit();
        gate.cancel();
    }
    // One deadline for the whole set, as in `CloseWindow`: every reader was
    // cancelled above, so they wind down in parallel and shutdown is bounded
    // once rather than once per session.
    let deadline = join_deadline();
    for (session_id, gate) in &gates {
        join_reader_bounded(*session_id, gate, deadline).await;
    }
}

/// Hand every live child to the incoming server on hot-reload.
///
/// Deliberately outside the close protocol in [`crate::session_exit`]: these
/// sessions are not ending. Cancelling their readers would drive the exit
/// funnel and publish `SessionExited` for panes the new server is about to
/// keep serving, so [`shutdown_pty_readers`] must not run on this path either.
/// These readers die with the process while the master fds live on in the new
/// one.
pub async fn defuse_for_handoff(live_sessions: &LiveSessionRegistry) {
    let mut sessions = live_sessions.write().await;
    for (&session_id, session) in sessions.iter_mut() {
        if let Some(pty) = session.take_pty() {
            // `defuse` leaks the inner `Pty` through `ManuallyDrop`, so
            // `Pty::drop` never runs and the child keeps living under the new
            // server, which already holds the master fd.
            pty.defuse();
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
    send_to_client(&client_writer, None, &msg);
    debug!(
        target: "scribe_server::ipc_server",
        ?session_id,
        ?internal_state,
        "forwarded EnvStatus transition to client"
    );
}

/// Spawn the agent-activity indicator forwarder (spec 027).
///
/// Consumes the [`crate::agent_api::activity::AgentActivityTracker`]'s
/// transition stream taken from the server's `agent_api` state and sends each
/// one as [`ServerMessage::AgentActivity`] to the owning window's
/// participants — capability-filtered, so a client that never advertised
/// `agent_api` in `Hello` receives nothing. The task ends when the tracker's
/// sender side (the `agent_api` state) is dropped at server shutdown.
pub fn spawn_agent_activity_forwarder(server: &IpcServerState) {
    let Some(mut transitions) = server.agent_api.take_activity_transitions() else {
        return;
    };
    let workspace_manager = Arc::clone(&server.workspace_manager);
    let window_shares = Arc::clone(&server.window_shares);
    tokio::spawn(async move {
        while let Some((session_id, active)) = transitions.recv().await {
            forward_agent_activity(&workspace_manager, &window_shares, session_id, active).await;
        }
    });
}

/// Helper for the agent-activity forwarder loop: resolves the session's
/// window and sends one transition to its `agent_api`-capable participants.
async fn forward_agent_activity(
    workspace_manager: &Arc<RwLock<WorkspaceManager>>,
    window_shares: &WindowShares,
    session_id: SessionId,
    active: bool,
) {
    let window_id = workspace_manager.read().await.window_for_session(session_id);
    let Some(window_id) = window_id else {
        // Session closed between the lease transition and our forward — drop
        // silently, mirroring `forward_env_status`.
        debug!(
            target: "scribe_server::ipc_server",
            ?session_id,
            "AgentActivity transition for unknown session — dropped"
        );
        return;
    };
    let msg = ServerMessage::AgentActivity { session_id, active };
    send_agent_api_message(window_shares, window_id, &msg).await;
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

/// The known windows in a stable order, so every decision taken over them is
/// reproducible.
///
/// `windows_with_sessions` is a `HashSet`, and walking it directly made both
/// the adoption pick and the spawn fan-out depend on hash iteration order —
/// the same class of bug the window session list had. An unnamed `Hello` (a
/// client that found no claimable snapshot) then adopted an arbitrary window,
/// differently on every process, so nothing downstream could line up with the
/// window it landed on.
fn windows_in_stable_order(windows_with_sessions: &HashSet<WindowId>) -> Vec<WindowId> {
    let mut ordered: Vec<WindowId> = windows_with_sessions.iter().copied().collect();
    ordered.sort_unstable_by_key(|wid| wid.to_full_string());
    ordered
}

/// Decide which `WindowId` to assign to a connecting client, and which
/// other unconnected windows should be spawned as separate processes.
///
/// When `hello_window_id` is `Some`, the client already knows its ID
/// (e.g. it was launched with `--window-id`) and may claim it only if no
/// current client owns that window. When `None`, this is a fresh launch — if
/// there are unconnected windows with sessions (restart scenario), the client
/// adopts one instead of creating a new ID, in
/// [`windows_in_stable_order`].
fn resolve_window_assignment<V>(
    hello_window_id: Option<WindowId>,
    windows_with_sessions: &HashSet<WindowId>,
    connected: &HashMap<WindowId, V>,
) -> (WindowId, Vec<WindowId>) {
    let ordered = windows_in_stable_order(windows_with_sessions);
    let next_unconnected = || {
        ordered
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

    let other_windows: Vec<WindowId> = ordered
        .into_iter()
        .filter(|wid| *wid != assigned && !connected.contains_key(wid))
        .collect();

    (assigned, other_windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
    use scribe_common::framing::{MAX_MESSAGE_SIZE, read_message};
    use scribe_common::protocol::{CiRunState, CiRunStatus};
    use scribe_common::terminal_images::{
        BoundedImageBytes, TerminalImageDataChunk, TerminalImageGeneration,
        TerminalImageRejectionReason, TerminalOutputSequence,
    };
    use std::os::unix::net::UnixStream as StdUnixStream;

    fn empty_output_queue() -> OutputQueueInner {
        OutputQueueInner {
            frames: VecDeque::new(),
            queued_pty_bytes: 0,
            queued_total_bytes: 0,
            dirty: HashSet::new(),
            closed: false,
        }
    }

    fn bare_output_sink() -> OutputSink {
        OutputSink(Arc::new(OutputQueueShared {
            inner: std::sync::Mutex::new(empty_output_queue()),
            notify: tokio::sync::Notify::new(),
            image_capabilities: std::sync::atomic::AtomicU32::new(0),
            pi_provider: AtomicBool::new(false),
        }))
    }

    /// One planned replay burst carrying `chunks` maximum-sized definition
    /// chunks, i.e. `chunks` MiB of canonical RGBA on the `Keep` lane.
    fn image_replay_records(session_id: SessionId, chunks: usize) -> Vec<ServerMessage> {
        let generation = TerminalImageGeneration(7);
        let sequence = TerminalOutputSequence(11);
        let chunk = BoundedImageBytes::new(vec![0; BoundedImageBytes::MAX_LEN])
            .expect("maximum replay chunk");
        let mut records = vec![ServerMessage::TerminalImageReplay {
            session_id,
            message: TerminalImageReplayMessage::Begin {
                generation,
                after_sequence: sequence,
                definition_count: 1,
                placement_count: 0,
                total_rgba_bytes: (chunks * BoundedImageBytes::MAX_LEN) as u64,
                active_screen: Some(TerminalScreenKind::Primary),
                rejection: None,
            },
        }];
        for index in 0..chunks {
            records.push(ServerMessage::TerminalImageReplay {
                session_id,
                message: TerminalImageReplayMessage::DefinitionChunk {
                    generation,
                    chunk: TerminalImageDataChunk {
                        id: scribe_common::terminal_images::TerminalImageId(1),
                        generation,
                        offset: (index * BoundedImageBytes::MAX_LEN) as u64,
                        data: chunk.clone(),
                        final_chunk: index + 1 == chunks,
                    },
                },
            });
        }
        records.push(ServerMessage::TerminalImageReplay {
            session_id,
            message: TerminalImageReplayMessage::Commit { generation, through_sequence: sequence },
        });
        records
    }

    /// A live sink owing a combined replay, plus the queue it writes into.
    fn sink_owing_a_replay(required: TerminalImageCapabilities) -> (OutputSink, ClientWriter) {
        let queue = bare_output_sink();
        queue.set_image_capabilities(required);
        let client_writer = Arc::new(std::sync::Mutex::new(AttachedSinks {
            sinks: vec![AttachedSink {
                writer: test_writer(),
                queue: queue.clone(),
                state: SinkState::Live,
                owes_image_replay: true,
            }],
        }));
        (queue, client_writer)
    }

    #[test]
    fn drop_pty_backlog_cannot_shrink_an_over_ceiling_keep_scene() {
        let session_id = SessionId::new();
        let mut queue = empty_output_queue();
        queue.frames.push_back(OutFrame::Session {
            session_id,
            bytes: 8,
            msg: ServerMessage::PtyOutput { session_id, data: vec![0; 8] },
        });
        queue.frames.push_back(OutFrame::Keep {
            bytes: OUTPUT_QUEUE_TOTAL_BYTES + 1,
            msg: ServerMessage::Error { message: String::from("oversized image replay") },
        });
        queue.queued_pty_bytes = 8;
        queue.queued_total_bytes = OUTPUT_QUEUE_TOTAL_BYTES + 9;

        drop_pty_backlog(&mut queue);

        assert_eq!(queue.frames.len(), 1);
        assert!(matches!(queue.frames.front(), Some(OutFrame::Keep { .. })));
        assert_eq!(queue.queued_pty_bytes, 0);
        assert_eq!(queue.queued_total_bytes, OUTPUT_QUEUE_TOTAL_BYTES + 1);
        assert!(queue.dirty.contains(&session_id));
    }

    #[test]
    fn enforce_queue_ceiling_closes_an_over_ceiling_keep_scene() {
        let mut queue = empty_output_queue();
        queue.frames.push_back(OutFrame::Keep {
            bytes: OUTPUT_QUEUE_TOTAL_BYTES + 1,
            msg: ServerMessage::Error { message: String::from("oversized image replay") },
        });
        queue.queued_total_bytes = OUTPUT_QUEUE_TOTAL_BYTES + 1;

        enforce_queue_ceiling(&mut queue);

        assert!(queue.closed, "the generic Keep policy cannot recover this shape");
    }

    #[test]
    fn screen_snapshot_is_charged_its_encoded_size() {
        let session_id = SessionId::new();
        let snapshot = ScreenSnapshot {
            cells: vec![ScreenCell {
                c: 'x',
                fg: scribe_common::screen::ScreenColor::Named(256),
                bg: scribe_common::screen::ScreenColor::Named(257),
                flags: scribe_common::screen::CellFlags::default(),
            }],
            cols: 1,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_style: scribe_common::screen::CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            active_dec_modes: Vec::new(),
            scrollback: Vec::new(),
            scrollback_rows: 0,
        };
        let message = ServerMessage::ScreenSnapshot { session_id, snapshot };
        let encoded = rmp_serde::to_vec_named(&message).expect("snapshot encodes");

        assert_eq!(out_frame_bytes(&message), encoded.len());
        assert_ne!(out_frame_bytes(&message), OUTPUT_FRAME_NOMINAL_BYTES);
    }

    #[tokio::test]
    async fn full_scrollback_snapshot_repair_encodes_and_reaches_the_client() {
        const COLS: u16 = 273;
        const ROWS: u16 = 24;
        const SCROLLBACK_ROWS: u32 = 10_000;
        let cell = ScreenCell {
            c: 'x',
            fg: scribe_common::screen::ScreenColor::Named(256),
            bg: scribe_common::screen::ScreenColor::Named(257),
            flags: scribe_common::screen::CellFlags::default(),
        };
        let snapshot = ScreenSnapshot {
            cells: vec![cell.clone(); usize::from(COLS) * usize::from(ROWS)],
            cols: COLS,
            rows: ROWS,
            cursor_col: 0,
            cursor_row: 0,
            cursor_style: scribe_common::screen::CursorStyle::Block,
            cursor_visible: true,
            alt_screen: false,
            active_dec_modes: Vec::new(),
            scrollback: vec![cell; usize::from(COLS) * SCROLLBACK_ROWS as usize],
            scrollback_rows: SCROLLBACK_ROWS,
        };
        let session_id = SessionId::new();
        let snapshot_message = ServerMessage::ScreenSnapshot { session_id, snapshot };
        let snapshot_bytes = rmp_serde::to_vec_named(&snapshot_message)
            .expect("full snapshot still serializes")
            .len();
        assert!(snapshot_bytes > MAX_MESSAGE_SIZE as usize, "fixture must reproduce the defect");
        let ServerMessage::ScreenSnapshot { snapshot: oversized, .. } = snapshot_message else {
            panic!("constructed snapshot message")
        };

        let replay = scribe_common::screen_replay::build_session_replay(&oversized)
            .expect("repair replay builds");
        let repair = ServerMessage::SessionReplay { session_id, replay };
        let repair_bytes = rmp_serde::to_vec_named(&repair).expect("repair encodes").len();
        assert!(repair_bytes <= MAX_MESSAGE_SIZE as usize);

        let (mut server, mut client) = tokio::io::duplex(1024 * 1024);
        assert!(write_queued_frame(&mut server, &repair).await);
        let received: ServerMessage =
            read_message(&mut client).await.expect("repair reached client");
        assert!(
            matches!(received, ServerMessage::SessionReplay { session_id: id, .. } if id == session_id)
        );
    }

    /// The repair `RequestSnapshot` asks for must be the bounded whole-pane
    /// replay, not the per-cell snapshot that outgrows `MAX_MESSAGE_SIZE` and is
    /// dropped before a byte reaches the socket.
    #[tokio::test]
    async fn request_snapshot_is_answered_with_the_bounded_whole_pane_replay() {
        let (server_sock, client_sock) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server_sock);
        let (mut client_read, _client_write) = tokio::io::split(client_sock);
        let writer = test_shared_writer(server_write);

        let (session_id, live_sessions, _slaves) = live_session_with_sink(120, 30, &writer).await;
        let attached: AttachedSessionIds =
            Arc::new(Mutex::new(std::iter::once(session_id).collect()));

        handle_request_snapshot(session_id, &writer, &live_sessions, &attached).await;

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_message::<ServerMessage, _>(&mut client_read),
        )
        .await
        .expect("the repair reached the client")
        .expect("the repair decoded");
        assert!(
            matches!(received, ServerMessage::SessionReplay { session_id: id, .. } if id == session_id),
            "RequestSnapshot must answer with a SessionReplay"
        );
    }

    /// A scene that still fits the Keep budget once the droppable backlog the
    /// queue may safely shed is ignored stays non-droppable and is queued whole.
    #[tokio::test]
    async fn a_fitting_replay_sheds_droppable_backlog_and_is_queued_whole() {
        let session_id = SessionId::new();
        let required = TerminalImageCapabilities::V1;
        let (queue, client_writer) = sink_owing_a_replay(required);
        queue.enqueue_session_frame(
            session_id,
            &ServerMessage::PtyOutput { session_id, data: vec![0; OUTPUT_QUEUE_PTY_BYTES] },
        );
        let records = image_replay_records(session_id, 13);
        let cost = keep_batch_cost(&records);
        assert!(cost <= OUTPUT_QUEUE_TOTAL_BYTES, "the scene must fit the Keep budget alone");
        assert!(
            cost + OUTPUT_QUEUE_PTY_BYTES > OUTPUT_QUEUE_TOTAL_BYTES,
            "but not alongside the droppable backlog"
        );

        assert_eq!(send_image_replay(&client_writer, required, &records), 1);

        let inner = queue.0.lock();
        assert!(!inner.closed);
        assert_eq!(inner.frames.len(), records.len(), "the whole scene is queued");
        assert_eq!(inner.queued_pty_bytes, 0, "the droppable backlog made the room");
        assert!(inner.dirty.contains(&session_id), "the shed session owes a text resync");
    }

    #[tokio::test]
    async fn oversized_image_replay_degrades_without_closing_across_attach_cycles() {
        let session_id = SessionId::new();
        let required = TerminalImageCapabilities::V1;
        let (queue, client_writer) = sink_owing_a_replay(required);
        let records = image_replay_records(session_id, 17);
        assert!(keep_batch_cost(&records) > OUTPUT_QUEUE_TOTAL_BYTES);

        for _ in 0..3 {
            assert_eq!(send_image_replay(&client_writer, required, &records), 1);
            let mut inner = queue.0.lock();
            assert!(!inner.closed);
            assert!(inner.queued_total_bytes < OUTPUT_QUEUE_TOTAL_BYTES);
            assert_eq!(inner.frames.len(), 2);
            assert!(matches!(
                inner.frames.front(),
                Some(OutFrame::Keep {
                    msg: ServerMessage::TerminalImageReplay {
                        message: TerminalImageReplayMessage::Begin {
                            definition_count: 0,
                            placement_count: 0,
                            rejection: Some(rejection),
                            ..
                        },
                        ..
                    },
                    ..
                }) if rejection.reason == TerminalImageRejectionReason::QuotaExceeded
            ));
            let drained = std::iter::from_fn(|| inner.pop_message()).count();
            assert_eq!(drained, 2, "the whole degraded burst reaches the drain task");
            drop(inner);
            lock_sinks(&client_writer).sinks[0].owes_image_replay = true;
        }
    }

    // @lat: [[test#Test Harness#Server lifecycle#Updater quit is a transient local action]]
    #[test]
    fn updater_quit_and_agent_requests_are_transient_local_actions() {
        assert!(is_transient_first_frame(&ClientMessage::QuitAll));
        assert!(is_transient_first_frame(&ClientMessage::AgentRequest(
            scribe_common::agent::AgentRequest::Capabilities {
                request_id: 1,
                agent_label: "test".into(),
                origin_session_id: None,
            },
        )));
    }

    // @lat: [[test#Test Harness#Pi Provider Compatibility#Remote and handoff version gates]]
    #[tokio::test]
    async fn remote_version_mismatch_returns_the_typed_refusal() {
        let (mut server, mut client) = tokio::io::duplex(4096);
        send_handshake_reply(
            &mut server,
            REMOTE_PROTOCOL_VERSION - 1,
            Some(RemoteRefusal::IncompatibleVersion),
        )
        .await;

        let reply: ServerMessage = read_message(&mut client).await.expect("handshake reply");
        assert!(matches!(
            reply,
            ServerMessage::RemoteHandshakeReply {
                accepted: false,
                refusal: Some(RemoteRefusal::IncompatibleVersion),
                server_remote_protocol_version,
                version_mismatch: Some(_),
                ..
            } if server_remote_protocol_version == REMOTE_PROTOCOL_VERSION
        ));
    }

    // @lat: [[protocol#Server Messages#Launch identity is local-only]]
    #[test]
    fn session_list_exposes_launch_identity_only_to_local_clients() {
        let local = WindowId::new();
        let other = WindowId::new();

        assert_eq!(
            list_session_launch_id(false, local, local, Some("launch-42")),
            Some("launch-42".to_owned())
        );
        assert_eq!(list_session_launch_id(true, local, local, Some("launch-42")), None);
        assert_eq!(list_session_launch_id(false, local, other, Some("launch-42")), None);
        assert_eq!(list_session_launch_id(false, local, local, None), None);

        assert!(requires_window_control(&ClientMessage::CreateSession {
            workspace_id: WorkspaceId::new(),
            split_direction: None,
            cwd: None,
            size: None,
            command: None,
            ai_launch: None,
            shell_tool: None,
            env_envelope_id: Some("launch-42".to_owned()),
        }));
    }

    // @lat: [[test#Test Harness#Pi Provider Compatibility#Local capability negotiation]]
    #[tokio::test]
    async fn old_client_output_downgrades_pi_and_never_sends_the_unknown_enum() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let session_id = SessionId::new();

        send_message(
            &writer,
            &ServerMessage::AiStateChanged {
                session_id,
                ai_state: AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing),
            },
        )
        .await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut client_read),
            )
            .await
            .is_err(),
            "an old client must not receive AiProvider::Pi"
        );

        let mut ai_state = AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing);
        ai_state.context = Some(55);
        send_message(
            &writer,
            &ServerMessage::SessionList {
                sessions: vec![SessionInfo {
                    session_id,
                    workspace_id: WorkspaceId::new(),
                    launch_id: Some("launch-pi".to_owned()),
                    shell_name: "bash".to_owned(),
                    title: None,
                    icon_title: None,
                    context: None,
                    task_label: Some("Pi task".to_owned()),
                    codex_task_label: None,
                    cwd: None,
                    git_branch: None,
                    ai_state: Some(ai_state),
                    ai_provider_hint: Some(AiProvider::Pi),
                    shell_tool: None,
                    prompt_state: Some(scribe_common::protocol::SessionPromptState::default()),
                }],
                workspace_tree: None,
                workspaces: Vec::new(),
            },
        )
        .await;
        let list: ServerMessage = read_message(&mut client_read).await.expect("legacy list frame");
        let ServerMessage::SessionList { sessions, .. } = list else {
            panic!("expected SessionList");
        };
        let session = sessions.first().expect("one session");
        assert_eq!(session.shell_tool, Some(ShellTool::Pi));
        assert!(session.ai_state.is_none());
        assert!(session.ai_provider_hint.is_none());
        assert!(session.task_label.is_none());
        assert!(session.prompt_state.is_none());

        writer.lock().await.queue().set_pi_provider_capability(true);
        send_message(
            &writer,
            &ServerMessage::AiStateChanged {
                session_id,
                ai_state: AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing),
            },
        )
        .await;
        let supported: ServerMessage =
            read_message(&mut client_read).await.expect("structured Pi frame");
        assert!(matches!(
            supported,
            ServerMessage::AiStateChanged { ai_state: received_state, .. }
                if received_state.provider == AiProvider::Pi
        ));
    }

    #[tokio::test]
    async fn old_client_output_drops_pi_task_label_and_prompt_events() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let session_id = SessionId::new();

        for msg in [
            ServerMessage::TaskLabelChanged {
                session_id,
                provider: AiProvider::Pi,
                task_label: "Pi task".to_owned(),
            },
            ServerMessage::TaskLabelCleared { session_id, provider: AiProvider::Pi },
            ServerMessage::PromptReceived {
                session_id,
                provider: AiProvider::Pi,
                text: "fix the bug".to_owned(),
            },
        ] {
            send_message(&writer, &msg).await;
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut client_read),
            )
            .await
            .is_err(),
            "an old client must not receive Pi task-label or prompt events"
        );

        writer.lock().await.queue().set_pi_provider_capability(true);
        send_message(
            &writer,
            &ServerMessage::TaskLabelChanged {
                session_id,
                provider: AiProvider::Pi,
                task_label: "Pi task".to_owned(),
            },
        )
        .await;
        let supported: ServerMessage =
            read_message(&mut client_read).await.expect("structured Pi frame");
        assert!(matches!(
            supported,
            ServerMessage::TaskLabelChanged { provider: AiProvider::Pi, .. }
        ));
    }

    // @lat: [[protocol#Server Messages#Launch identity is local-only]]
    #[tokio::test]
    async fn create_session_requires_the_current_window_owner() {
        let local = test_writer();
        let remote = test_writer();
        let lost_control = test_writer();
        let window_id = WindowId::new();
        let mut share = WindowShare::new(
            Participant::local(&local, false),
            scribe_config::SharingMode::FreeForAll,
            scribe_config::ControlAcquisition::FreeClaim,
            None,
        );
        share.add_participant(Participant::new(
            &remote,
            ControllerIdentity::Remote {
                device_name: "peer".to_owned(),
                login_name: "viewer".to_owned(),
            },
            ParticipantTransport::Remote,
            false,
            AgentApiCapability::Unsupported,
        ));
        let shares = new_window_shares();
        shares.write().await.insert(window_id, share);
        let create = ClientMessage::CreateSession {
            workspace_id: WorkspaceId::new(),
            split_direction: None,
            cwd: None,
            size: None,
            command: None,
            ai_launch: None,
            shell_tool: None,
            env_envelope_id: Some("launch-42".to_owned()),
        };

        assert!(connection_may_type(&shares, window_id, &local, &create).await);
        assert!(!connection_may_type(&shares, window_id, &remote, &create).await);
        assert!(!connection_may_type(&shares, window_id, &lost_control, &create).await);
    }

    #[tokio::test]
    async fn beads_detail_and_flow_reject_remote_viewers_and_mismatched_workspaces() {
        let base = Path::new("/work");
        let repo = base.join("scribe");
        let window_id = WindowId::new();
        let shared_window = WindowId::new();
        let other_window = WindowId::new();
        let local = test_writer();
        let shared_owner = test_writer();
        let remote = test_writer();
        let mut share = WindowShare::new(
            Participant::local(&shared_owner, false),
            scribe_config::SharingMode::FreeForAll,
            scribe_config::ControlAcquisition::FreeClaim,
            None,
        );
        share.add_participant(Participant::new(
            &remote,
            ControllerIdentity::Remote {
                device_name: "peer".to_owned(),
                login_name: "viewer".to_owned(),
            },
            ParticipantTransport::Remote,
            false,
            AgentApiCapability::Unsupported,
        ));
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(
                window_id,
                WindowShare::new_single_controller(Participant::local(&local, false)),
            );
            shares.insert(shared_window, share);
        }

        let mut manager = WorkspaceManager::new(vec![base.to_path_buf()]);
        let workspace_id = manager.create_workspace();
        let session_id = SessionId::new();
        manager.add_session(workspace_id, session_id, None);
        manager.assign_session_to_window(window_id, session_id);
        manager.on_cwd_changed(session_id, &repo.join("src"));
        let manager = Arc::new(RwLock::new(manager));

        assert_eq!(
            // Flow calls this exact helper, so the local-owner and workspace
            // proof covers both request types without a second authorization path.
            beads_detail_request_root(
                &manager,
                &shares,
                BeadsDetailRequest { window_id, writer: &local, is_remote: false, workspace_id },
            )
            .await,
            Some(repo)
        );
        {
            let shares = shares.read().await;
            let shared_window_state = shares.get(&shared_window);
            assert!(!beads_detail_connection_available(shared_window_state, &shared_owner, false));
            assert!(!beads_detail_connection_available(shared_window_state, &remote, true));
        }
        assert!(
            beads_detail_request_root(
                &manager,
                &shares,
                BeadsDetailRequest {
                    window_id: other_window,
                    writer: &local,
                    is_remote: false,
                    workspace_id,
                },
            )
            .await
            .is_none()
        );
    }

    // @lat: [[lat.md/test#Test Harness#Server handoff#Upgrade takeover replaces a live socket]]
    #[tokio::test]
    async fn upgrade_takeover_replaces_a_live_socket_in_place() {
        let dir = std::env::temp_dir().join(format!("scribe-takeover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("server.sock");
        drop(std::fs::remove_file(&path));
        let staging = staging_socket_path(&path);
        drop(std::fs::remove_file(&staging));

        // Stand in for the old server: bound and listening, exactly as it still
        // is when the receiver takes the path over ahead of the handoff ACK.
        let old = std::os::unix::net::UnixListener::bind(&path).expect("old server binds");
        // Stand in for a receiver killed after staging its socket but before
        // rename. The next receiver must clear this orphan before binding.
        let stale = std::os::unix::net::UnixListener::bind(&staging).expect("stale staging binds");

        let (lock, successor) =
            acquire_server_socket(&path, true).expect("takeover over a live socket");

        assert!(lock.is_none(), "an upgrade receiver must not take the singleton lock");
        assert!(!staging_socket_path(&path).exists(), "staging socket left behind");
        assert!(path.exists(), "the socket path must survive the takeover");

        // The path now routes to the successor. Proving the old listener is
        // bypassed is the point: its inode is orphaned by the rename, so a
        // client that connects after the takeover can only reach the new server.
        let _client =
            std::os::unix::net::UnixStream::connect(&path).expect("connect after takeover");
        successor.accept().await.expect("successor accepts the connection");
        old.set_nonblocking(true).expect("nonblocking");
        assert!(old.accept().is_err(), "the orphaned listener must receive nothing");
        stale.set_nonblocking(true).expect("nonblocking");
        assert!(stale.accept().is_err(), "the stale staging listener must receive nothing");

        drop(old);
        drop(stale);
        drop(std::fs::remove_dir_all(&dir));
    }

    // @lat: [[lat.md/server#Server#Sessions#Retained Prompt History#Retained prompt history]]
    #[test]
    fn retained_prompt_history_latches_the_first_and_tracks_the_latest() {
        use std::time::{Duration, UNIX_EPOCH};

        let mut state: Option<scribe_common::protocol::SessionPromptState> = None;
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        state.get_or_insert_with(Default::default).record_prompt("build the thing", at);
        state
            .get_or_insert_with(Default::default)
            .record_prompt("now ship it", at + Duration::from_secs(30));

        let mut prompts = state.expect("a submitted prompt creates the record");
        assert_eq!(prompts.prompt_count, 2);
        assert_eq!(prompts.first_prompt.as_deref(), Some("build the thing"));
        assert_eq!(prompts.latest_prompt.as_deref(), Some("now ship it"));
        assert_eq!(prompts.latest_prompt_at, Some(1_700_000_030));

        // The run ends: the timer freezes once and idle edges do not push it.
        prompts.note_prompt_progress(&AiState::IdlePrompt, at + Duration::from_secs(75));
        prompts.note_prompt_progress(&AiState::WaitingForInput, at + Duration::from_mins(10));
        assert_eq!(prompts.latest_prompt_finished_at, Some(1_700_000_075));

        // Back to work: the timer runs again.
        prompts.note_prompt_progress(&AiState::Processing, at + Duration::from_mins(12));
        assert_eq!(prompts.latest_prompt_finished_at, None);
    }

    fn unix_stream_pair() -> (tokio::net::UnixStream, tokio::net::UnixStream) {
        let (left, right) = StdUnixStream::pair().unwrap();
        left.set_nonblocking(true).unwrap();
        right.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(left).unwrap(),
            tokio::net::UnixStream::from_std(right).unwrap(),
        )
    }

    fn ci_test_writer() -> (SharedWriter, tokio::net::UnixStream) {
        let (server, client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        (test_shared_writer(write), client)
    }

    fn ci_share(writer: &SharedWriter, capable: bool) -> WindowShare {
        let controller = ControllerIdentity::Local;
        let claim = HelloClaim {
            requested_window_id: None,
            clipboard_gating: false,
            intent: ClaimIntent::Plain,
            terminal_images: TerminalImageCapabilities::default(),
            ci_run_bar: capable,
            agent_api: AgentApiCapability::Unsupported,
            controller: &controller,
        };
        WindowShare::new_single_controller(Participant::from_claim(&claim, writer))
    }

    fn agent_share(writer: &SharedWriter, capable: bool) -> WindowShare {
        let controller = ControllerIdentity::Local;
        let claim = HelloClaim {
            requested_window_id: None,
            clipboard_gating: false,
            intent: ClaimIntent::Plain,
            terminal_images: TerminalImageCapabilities::default(),
            ci_run_bar: false,
            agent_api: capable.into(),
            controller: &controller,
        };
        WindowShare::new_single_controller(Participant::from_claim(&claim, writer))
    }

    fn add_ci_workspace(
        manager: &mut WorkspaceManager,
        window_id: WindowId,
        cwd: &Path,
    ) -> WorkspaceId {
        let workspace_id = manager.create_workspace();
        let session_id = SessionId::new();
        manager.add_session(workspace_id, session_id, None);
        manager.assign_session_to_window(window_id, session_id);
        manager.on_cwd_changed(session_id, cwd);
        workspace_id
    }

    // @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh#Server Beads Issue Writes#Root fan-out]]
    #[tokio::test]
    async fn beads_board_refresh_reaches_each_authorized_same_root_workspace() {
        let base = Path::new("/work");
        let repo = base.join("scribe");
        let other_repo = base.join("other");
        let first_window = WindowId::new();
        let second_window = WindowId::new();
        let other_window = WindowId::new();
        let mut manager = WorkspaceManager::new(vec![base.to_path_buf()]);
        let first_workspace = add_ci_workspace(&mut manager, first_window, &repo.join("src"));
        let second_workspace = add_ci_workspace(&mut manager, second_window, &repo.join("tests"));
        add_ci_workspace(&mut manager, other_window, &other_repo.join("src"));
        let manager = Arc::new(RwLock::new(manager));

        let (first_writer, mut first_client) = ci_test_writer();
        let (second_writer, mut second_client) = ci_test_writer();
        let (other_writer, mut other_client) = ci_test_writer();
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(first_window, ci_share(&first_writer, false));
            shares.insert(second_window, ci_share(&second_writer, false));
            shares.insert(other_window, ci_share(&other_writer, false));
        }

        push_beads_board_for_root(
            &manager,
            &shares,
            &repo,
            BeadsBoardState::Unavailable { message: "refreshed".into() },
        )
        .await;

        for (client, expected_workspace) in
            [(&mut first_client, first_workspace), (&mut second_client, second_workspace)]
        {
            let message: ServerMessage = read_message(client).await.unwrap();
            assert!(matches!(
                message,
                ServerMessage::BeadsBoard {
                    workspace_id,
                    protocol_version: BEADS_BOARD_PROTOCOL_VERSION,
                    state: BeadsBoardState::Unavailable { message },
                } if workspace_id == expected_workspace && message == "refreshed"
            ));
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut other_client),
            )
            .await
            .is_err(),
            "a workspace rooted in another repo received the Beads board"
        );
    }

    fn ci_state(head_sha: &str) -> CiRunState {
        CiRunState {
            repository: "acme/scribe".into(),
            head_sha: head_sha.into(),
            branch: "main".into(),
            workflows: Vec::new(),
            rollup: CiRunStatus::Queued,
            stale: false,
        }
    }

    #[tokio::test]
    async fn agent_frames_reach_only_participants_that_advertise_agent_api() {
        let capable_window = WindowId::new();
        let incapable_window = WindowId::new();
        let (capable_writer, mut capable_client) = ci_test_writer();
        let (incapable_writer, mut incapable_client) = ci_test_writer();
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(capable_window, agent_share(&capable_writer, true));
            shares.insert(incapable_window, agent_share(&incapable_writer, false));
        }

        let activity = ServerMessage::AgentActivity { session_id: SessionId::new(), active: true };
        super::send_agent_api_message(&shares, capable_window, &activity).await;
        super::send_agent_api_message(&shares, incapable_window, &activity).await;
        assert!(matches!(
            read_message(&mut capable_client).await.unwrap(),
            ServerMessage::AgentActivity { active: true, .. }
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut incapable_client),
            )
            .await
            .is_err()
        );

        let prompt = ServerMessage::AgentPromptRequest {
            prompt_id: scribe_common::protocol::PromptId(9),
            agent_label: "test-agent".into(),
            capability: scribe_common::agent::AgentCapability::ReadMetadata,
            target: "server".into(),
        };
        super::send_agent_api_message(&shares, capable_window, &prompt).await;
        super::send_agent_api_message(&shares, incapable_window, &prompt).await;
        assert!(matches!(
            read_message(&mut capable_client).await.unwrap(),
            ServerMessage::AgentPromptRequest {
                prompt_id: scribe_common::protocol::PromptId(9),
                ..
            }
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut incapable_client),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn agent_activity_transitions_route_to_the_owning_windows_capable_participants() {
        let capable_window = WindowId::new();
        let incapable_window = WindowId::new();
        let mut manager = WorkspaceManager::new(Vec::new());
        let capable_session = SessionId::new();
        let capable_workspace = manager.create_workspace();
        manager.add_session(capable_workspace, capable_session, None);
        manager.assign_session_to_window(capable_window, capable_session);
        let incapable_session = SessionId::new();
        let incapable_workspace = manager.create_workspace();
        manager.add_session(incapable_workspace, incapable_session, None);
        manager.assign_session_to_window(incapable_window, incapable_session);
        let manager = Arc::new(RwLock::new(manager));

        let (capable_writer, mut capable_client) = ci_test_writer();
        let (incapable_writer, mut incapable_client) = ci_test_writer();
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(capable_window, agent_share(&capable_writer, true));
            shares.insert(incapable_window, agent_share(&incapable_writer, false));
        }

        // Unknown session: dropped without reaching any participant.
        super::forward_agent_activity(&manager, &shares, SessionId::new(), true).await;
        super::forward_agent_activity(&manager, &shares, capable_session, true).await;
        super::forward_agent_activity(&manager, &shares, incapable_session, true).await;

        assert!(matches!(
            read_message(&mut capable_client).await.unwrap(),
            ServerMessage::AgentActivity { session_id, active: true }
                if session_id == capable_session
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut incapable_client),
            )
            .await
            .is_err(),
            "a participant without the agent_api bit received an AgentActivity frame"
        );
    }

    // @lat: [[protocol#Server Messages#CI Run State#Capability and repository scoping]]
    #[tokio::test]
    async fn ci_updates_reach_only_capable_clients_rooted_in_the_repo() {
        let base = Path::new("/work");
        let repo = base.join("scribe");
        let other_repo = base.join("other");
        let capable_window = WindowId::new();
        let incapable_window = WindowId::new();
        let other_window = WindowId::new();
        let mut manager = WorkspaceManager::new(vec![base.to_path_buf()]);
        add_ci_workspace(&mut manager, capable_window, &repo.join("src"));
        add_ci_workspace(&mut manager, incapable_window, &repo.join("tests"));
        add_ci_workspace(&mut manager, other_window, &other_repo.join("src"));
        let manager = Arc::new(RwLock::new(manager));

        let (capable_writer, mut capable_client) = ci_test_writer();
        let (incapable_writer, mut incapable_client) = ci_test_writer();
        let (other_writer, mut other_client) = ci_test_writer();
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(capable_window, ci_share(&capable_writer, true));
            shares.insert(incapable_window, ci_share(&incapable_writer, false));
            shares.insert(other_window, ci_share(&other_writer, true));
        }

        publish_ci_run_delta(
            &manager,
            &shares,
            &CiDismissals::default(),
            &repo,
            CiRunDelta::Set(ci_state("head-a")),
        )
        .await;

        let message: ServerMessage = read_message(&mut capable_client).await.unwrap();
        assert!(matches!(
            message,
            ServerMessage::CiRunState { repo_root, delta: CiRunDelta::Set(state) }
                if repo_root == repo && state.head_sha == "head-a"
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut incapable_client),
            )
            .await
            .is_err(),
            "an incapable client received an unknown CI frame"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut other_client),
            )
            .await
            .is_err(),
            "a window rooted in another repo received the update"
        );
    }

    // @lat: [[test#GitHub CI Tracking#Detail interest authorization]]
    #[tokio::test]
    async fn ci_detail_interest_allows_capable_viewers_only_for_visible_roots() {
        let base = Path::new("/work");
        let repo = base.join("scribe");
        let window_id = WindowId::new();
        let mut manager = WorkspaceManager::new(vec![base.to_path_buf()]);
        add_ci_workspace(&mut manager, window_id, &repo.join("src"));
        let manager = Arc::new(RwLock::new(manager));
        let (writer, _client) = ci_test_writer();
        let shares = new_window_shares();
        shares.write().await.insert(window_id, ci_share(&writer, true));

        assert!(
            ci_detail_interest_allowed(
                &manager,
                &shares,
                CiDetailInterestRequest {
                    window_id,
                    writer: &writer,
                    repo_root: &repo,
                    interested: true,
                },
            )
            .await
        );
        assert!(
            !ci_detail_interest_allowed(
                &manager,
                &shares,
                CiDetailInterestRequest {
                    window_id,
                    writer: &writer,
                    repo_root: &base.join("other"),
                    interested: true,
                },
            )
            .await
        );
        assert!(
            ci_detail_interest_allowed(
                &manager,
                &shares,
                CiDetailInterestRequest {
                    window_id,
                    writer: &writer,
                    repo_root: &base.join("old-root"),
                    interested: false,
                },
            )
            .await
        );
    }

    // @lat: [[protocol#Server Messages#CI Run State#Synchronized dismissal]]
    #[tokio::test]
    async fn dismissing_a_head_clears_all_capable_views_until_a_new_head() {
        let base = Path::new("/work");
        let repo = base.join("scribe");
        let sender_window = WindowId::new();
        let peer_window = WindowId::new();
        let mut manager = WorkspaceManager::new(vec![base.to_path_buf()]);
        add_ci_workspace(&mut manager, sender_window, &repo.join("src"));
        add_ci_workspace(&mut manager, peer_window, &repo.join("tests"));
        let manager = Arc::new(RwLock::new(manager));

        let (sender_writer, mut sender_client) = ci_test_writer();
        let (peer_writer, mut peer_client) = ci_test_writer();
        let shares = new_window_shares();
        {
            let mut shares = shares.write().await;
            shares.insert(sender_window, ci_share(&sender_writer, true));
            shares.insert(peer_window, ci_share(&peer_writer, true));
        }
        let dismissals = CiDismissals::default();

        apply_ci_dismissal(
            &manager,
            &shares,
            &dismissals,
            CiDismissRequest {
                window_id: sender_window,
                writer: &sender_writer,
                is_remote: false,
                repo_root: repo.clone(),
                head_sha: "head-a".into(),
            },
        )
        .await;

        for client in [&mut sender_client, &mut peer_client] {
            let message: ServerMessage = read_message(client).await.unwrap();
            assert!(matches!(
                message,
                ServerMessage::CiRunState {
                    ref repo_root,
                    delta: CiRunDelta::Cleared { ref head_sha }
                } if repo_root == &repo && head_sha == "head-a"
            ));
        }
        assert_eq!(dismissals.read().await.get(&repo).map(String::as_str), Some("head-a"));

        publish_ci_run_delta(
            &manager,
            &shares,
            &dismissals,
            &repo,
            CiRunDelta::Set(ci_state("head-a")),
        )
        .await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_message::<ServerMessage, _>(&mut peer_client),
            )
            .await
            .is_err(),
            "the dismissed head reappeared"
        );

        publish_ci_run_delta(
            &manager,
            &shares,
            &dismissals,
            &repo,
            CiRunDelta::Set(ci_state("head-b")),
        )
        .await;
        let message: ServerMessage = read_message(&mut peer_client).await.unwrap();
        assert!(matches!(
            message,
            ServerMessage::CiRunState { delta: CiRunDelta::Set(state), .. }
                if state.head_sha == "head-b"
        ));
        assert!(!dismissals.read().await.contains_key(&repo));
    }

    /// Drive one buffering sink and read back what actually reached its socket.
    async fn buffered_attach_frames(
        emitted: &[(Option<u64>, ServerMessage)],
        snapshot_commit: u64,
    ) -> (AttachedSinks, SharedWriter, tokio::io::ReadHalf<tokio::net::UnixStream>) {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let queue = writer.lock().await.queue();

        let mut sinks = AttachedSinks::default();
        sinks.begin_attach(&writer, queue, false);
        for (commit, msg) in emitted {
            sinks.fan_out(*commit, msg);
        }
        sinks.finish_attach(&writer, snapshot_commit, SessionId::new());
        (sinks, writer, client_read)
    }

    fn pty_frame(session_id: SessionId, data: &str) -> ServerMessage {
        ServerMessage::PtyOutput { session_id, data: data.as_bytes().to_vec() }
    }

    /// A buffering sink must drop exactly the frames its replay snapshot already
    /// carries, keep every frame the snapshot cannot carry, and flush the
    /// survivors in emission order — the whole point of the commit cursor.
    #[tokio::test]
    async fn buffering_sink_drops_snapshotted_frames_and_flushes_the_rest_in_order() {
        let session_id = SessionId::new();
        let emitted = vec![
            (Some(10), pty_frame(session_id, "before")),
            (Some(20), ServerMessage::TrimScrollback { session_id, history_rows: 7 }),
            (None, ServerMessage::TitleChanged { session_id, title: String::from("t") }),
            (Some(30), pty_frame(session_id, "after")),
            (Some(30), ServerMessage::ScrollBottom { session_id }),
        ];
        let (mut sinks, _writer, mut client_read) = buffered_attach_frames(&emitted, 20).await;

        // Everything at or below the snapshot's cursor is already in the replay.
        let first = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(first, ServerMessage::TitleChanged { .. }),
            "untagged frames always flush, got {first:?}"
        );
        let second = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(&second, ServerMessage::PtyOutput { data, .. } if data == b"after"),
            "frames past the snapshot flush in emission order, got {second:?}"
        );
        let third = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(third, ServerMessage::ScrollBottom { .. }),
            "a bottom-snap rides its chunk's cursor, got {third:?}"
        );

        // The sink is Live now: further output goes straight through.
        sinks.fan_out(Some(40), &pty_frame(session_id, "live"));
        let fourth = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(&fourth, ServerMessage::PtyOutput { data, .. } if data == b"live"),
            "post-flush output goes straight to the queue, got {fourth:?}"
        );
    }

    /// A pre-replay buffer that outgrows its budget must not hand the client a
    /// truncated backlog: it sheds everything and leans on the resync replay.
    #[tokio::test]
    async fn overflowing_attach_buffer_sheds_its_backlog_instead_of_replaying_it() {
        let session_id = SessionId::new();
        let huge = "x".repeat(OUTPUT_QUEUE_PTY_BYTES / 2 + 1);
        let emitted = vec![
            (Some(10), pty_frame(session_id, &huge)),
            (Some(20), pty_frame(session_id, &huge)),
            (Some(30), pty_frame(session_id, "shed-too")),
        ];
        let (mut sinks, _writer, mut client_read) = buffered_attach_frames(&emitted, 0).await;

        sinks.fan_out(Some(40), &pty_frame(session_id, "live"));
        let first = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(&first, ServerMessage::PtyOutput { data, .. } if data == b"live"),
            "the shed backlog must not reach the wire, got {first:?}"
        );
    }

    /// A filter that swallows a whole chunk must not put a zero-byte
    /// `PtyOutput` on the wire. The companion frame proves the drop happened at
    /// the framing step rather than somewhere the queue would have hidden it.
    #[tokio::test]
    async fn empty_pty_chunk_is_never_framed() {
        let session_id = SessionId::new();
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let queue = writer.lock().await.queue();

        let mut sinks = AttachedSinks::default();
        sinks.begin_attach(&writer, queue, false);
        sinks.finish_attach(&writer, 0, session_id);
        let client_writer: ClientWriter = Arc::new(std::sync::Mutex::new(sinks));

        send_pty_output(&client_writer, session_id, b"", 10);
        send_to_client(&client_writer, Some(10), &ServerMessage::ScrollBottom { session_id });

        let first = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        assert!(
            matches!(first, ServerMessage::ScrollBottom { .. }),
            "an empty chunk must not be framed at all, got {first:?}"
        );
    }

    #[tokio::test]
    async fn suppressed_ai_ed3_forwards_filtered_output_without_scroll_bottom() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, _live_sessions, slaves) = live_session_with_sink(80, 24, &writer).await;

        nix::unistd::write(
            &slaves[0],
            b"\x1b]1337;ScribeAiLaunch=claude_code\x07before\x1b[3Jafter",
        )
        .unwrap();

        let first: ServerMessage =
            tokio::time::timeout(std::time::Duration::from_secs(3), read_message(&mut client_read))
                .await
                .expect("filtered output arrived")
                .expect("filtered output decoded");
        assert!(matches!(
            first,
            ServerMessage::PtyOutput { session_id: id, data }
                if id == session_id
                    && data.windows(4).all(|window| window != b"\x1b[3J")
                    && data.ends_with(b"beforeafter")
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                read_message::<ServerMessage, _>(&mut client_read),
            )
            .await
            .is_err(),
            "a suppressed ED 3 must not synthesize ScrollBottom after its output"
        );
    }

    #[tokio::test]
    async fn non_ai_ed3_stays_on_the_pty_output_wire() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, _live_sessions, slaves) = live_session_with_sink(80, 24, &writer).await;

        nix::unistd::write(&slaves[0], b"before\x1b[3Jafter").unwrap();

        let message: ServerMessage =
            tokio::time::timeout(std::time::Duration::from_secs(3), read_message(&mut client_read))
                .await
                .expect("plain ED 3 arrived")
                .expect("plain ED 3 decoded");
        assert!(matches!(
            message,
            ServerMessage::PtyOutput { session_id: id, data }
                if id == session_id && data.ends_with(b"before\x1b[3Jafter")
        ));
    }

    #[tokio::test]
    async fn plain_pty_append_stays_a_single_output_frame() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, _live_sessions, slaves) = live_session_with_sink(80, 24, &writer).await;

        nix::unistd::write(&slaves[0], b"plain append").unwrap();

        let message: ServerMessage =
            tokio::time::timeout(std::time::Duration::from_secs(3), read_message(&mut client_read))
                .await
                .expect("plain append arrived")
                .expect("plain append decoded");
        assert!(matches!(
            message,
            ServerMessage::PtyOutput { session_id: id, data }
                if id == session_id && data.ends_with(b"plain append")
        ));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                read_message::<ServerMessage, _>(&mut client_read),
            )
            .await
            .is_err(),
            "a plain append must not add a synthetic positional frame"
        );
    }

    /// Stand up one live session over a real PTY pair, with `writer`'s sink as
    /// its sole attached client.
    ///
    /// The handoff-restore path is the only way to reach a `LiveSession` without
    /// forking a child: it takes the master fd we opened here and wires the same
    /// registry entry `start_session` builds for a spawned shell. The returned
    /// slave fds must outlive the test — dropping one EOFs the reader task,
    /// which unwires the session.
    async fn live_session_with_sink(
        cols: u16,
        rows: u16,
        writer: &SharedWriter,
    ) -> (SessionId, LiveSessionRegistry, Vec<std::os::fd::OwnedFd>) {
        let pty = nix::pty::openpty(None, None).unwrap();
        let session_id = SessionId::new();
        let state = crate::handoff::HandoffState {
            version: 5,
            sessions: vec![crate::handoff::HandoffSession {
                session_id,
                workspace_id: WorkspaceId::new(),
                child_pid: std::process::id(),
                child_identity: None,
                cols,
                rows,
                cell_width: 1,
                cell_height: 1,
                snapshot: None,
                session_replay: None,
                title: None,
                icon_title: None,
                shell_name: String::from("bash"),
                task_label: None,
                codex_task_label: None,
                cwd: None,
                context: None,
                ai_state: None,
                ai_provider_hint: None,
                shell_tool: None,
                prompt_state: None,
                env_window_id: None,
                env_envelope_id: None,
                image_state: None,
            }],
            workspaces: vec![],
            workspace_tree: None,
            windows: vec![],
            ci_windows: vec![],
        };

        let manager = SessionManager::restore_from_handoff(&state, vec![pty.master], 100).unwrap();
        let workspaces = Arc::new(RwLock::new(WorkspaceManager::new(vec![])));
        let live_sessions = new_live_session_registry();
        let shares = new_window_shares();
        let git_ref_watcher = Arc::new(GitRefWatcherControl::new(false));
        activate_pending_sessions(&manager, &workspaces, &live_sessions, &shares, &git_ref_watcher)
            .await;

        for _ in 0..100 {
            if live_sessions.read().await.contains_key(&session_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let queue = writer.lock().await.queue();
        {
            let sessions = live_sessions.read().await;
            let session = sessions.get(&session_id).expect("session reached the live registry");
            lock_sinks(&session.client_writer).set_sole(Arc::clone(writer), queue);
        }
        (session_id, live_sessions, vec![pty.slave])
    }

    async fn install_flow_owner(
        session_id: SessionId,
        live_sessions: &LiveSessionRegistry,
        writer: &SharedWriter,
    ) -> WindowShares {
        let window_id = WindowId::new();
        live_sessions.write().await.get_mut(&session_id).expect("live session").env_window_id =
            window_id;
        let shares = new_window_shares();
        shares.write().await.insert(
            window_id,
            WindowShare::new_single_controller(Participant::local(writer, false)),
        );
        shares
    }

    // @lat: [[server#Beads Flow source cache#Focused issue liveness]]
    #[tokio::test]
    async fn focused_issue_reaches_only_the_local_flow_owner_and_clears() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &writer).await;
        let shares = install_flow_owner(session_id, &live_sessions, &writer).await;

        set_focused_issue(session_id, Some(String::from("scribe-lpi2.8")), &live_sessions, &shares)
            .await;
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::IssueFocused { session_id: id, issue_id: Some(issue) }
                if id == session_id && issue == "scribe-lpi2.8"
        ));

        set_focused_issue(session_id, None, &live_sessions, &shares).await;
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::IssueFocused { session_id: id, issue_id: None } if id == session_id
        ));
        assert!(live_sessions.read().await[&session_id].focused_issue.is_none());
    }

    // @lat: [[server#Beads Flow source cache#Focused issue liveness]]
    #[tokio::test]
    async fn focused_issue_drops_unknown_and_withholds_remote_or_shared_delivery() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &writer).await;
        let shares = install_flow_owner(session_id, &live_sessions, &writer).await;

        set_focused_issue(
            SessionId::new(),
            Some(String::from("scribe-missing")),
            &live_sessions,
            &shares,
        )
        .await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                read_message::<ServerMessage, _>(&mut client_read)
            )
            .await
            .is_err(),
            "an unknown session must not receive a frame"
        );

        let window_id = live_sessions.read().await[&session_id].env_window_id;
        shares.write().await.insert(
            window_id,
            WindowShare::new(
                Participant::local(&writer, false),
                scribe_config::SharingMode::FreeForAll,
                scribe_config::ControlAcquisition::FreeClaim,
                None,
            ),
        );
        set_focused_issue(session_id, Some(String::from("scribe-lpi2.8")), &live_sessions, &shares)
            .await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                read_message::<ServerMessage, _>(&mut client_read)
            )
            .await
            .is_err(),
            "a shared participant must not receive a liveness frame"
        );

        let remote = Participant::new(
            &writer,
            ControllerIdentity::Remote {
                device_name: String::from("remote-device"),
                login_name: String::from("remote-account"),
            },
            ParticipantTransport::Remote,
            false,
            AgentApiCapability::Unsupported,
        );
        shares.write().await.insert(window_id, WindowShare::new_single_controller(remote));
        set_focused_issue(session_id, None, &live_sessions, &shares).await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                read_message::<ServerMessage, _>(&mut client_read)
            )
            .await
            .is_err(),
            "a remote participant must not receive a liveness frame"
        );
    }

    // @lat: [[server#Beads Flow source cache#Focused issue liveness]]
    #[tokio::test]
    async fn state_clear_and_session_exit_clear_the_focused_issue() {
        let (server, client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let writer = test_shared_writer(server_write);
        let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &writer).await;
        let shares = install_flow_owner(session_id, &live_sessions, &writer).await;
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(Vec::new())));
        let git_ref_watcher = Arc::new(GitRefWatcherControl::new(false));
        let client_writer = Arc::clone(&live_sessions.read().await[&session_id].client_writer);

        set_focused_issue(session_id, Some(String::from("scribe-lpi2.8")), &live_sessions, &shares)
            .await;
        let _ = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        send_metadata_event(
            MetadataEvent::AiStateCleared,
            session_id,
            &client_writer,
            MetadataRuntime {
                workspace_manager: &workspace_manager,
                live_sessions: &live_sessions,
                window_shares: &shares,
                git_ref_watcher: &git_ref_watcher,
            },
        )
        .await;
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::AiStateCleared { session_id: id } if id == session_id
        ));
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::IssueFocused { session_id: id, issue_id: None } if id == session_id
        ));

        set_focused_issue(session_id, Some(String::from("scribe-lpi2.8")), &live_sessions, &shares)
            .await;
        let _ = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        let window_id = live_sessions.read().await[&session_id].env_window_id;
        let attached_ids = HashSet::from([session_id]);
        clear_focused_issues_for_disconnect(
            &live_sessions,
            &shares,
            window_id,
            &attached_ids,
            &writer,
        )
        .await;
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::IssueFocused { session_id: id, issue_id: None } if id == session_id
        ));

        set_focused_issue(session_id, Some(String::from("scribe-lpi2.8")), &live_sessions, &shares)
            .await;
        let _ = read_message::<ServerMessage, _>(&mut client_read).await.unwrap();
        let handles = live_sessions.read().await[&session_id].exit_handles();
        finalize_session_exit(
            &handles.exit_gate,
            SessionExitContext {
                session_id,
                client_writer: &handles.client_writer,
                attachment: &handles.attachment,
                live_sessions: &live_sessions,
                window_shares: &shares,
                workspace_manager: &workspace_manager,
            },
            ChildExit::UNKNOWN,
        )
        .await;
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::IssueFocused { session_id: id, issue_id: None } if id == session_id
        ));
        assert!(matches!(
            read_message::<ServerMessage, _>(&mut client_read).await.unwrap(),
            ServerMessage::SessionExited { session_id: id, .. } if id == session_id
        ));
        assert!(!live_sessions.read().await.contains_key(&session_id));
    }

    // @lat: [[lat.md/server#Server#Sessions#Retained Prompt History#SessionEnd clears reattach chrome]]
    #[tokio::test]
    async fn state_cleared_drops_all_reattach_chrome_after_attention_dismissal() {
        let (server, _client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let writer = test_shared_writer(server_write);
        let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &writer).await;

        persist_session_metadata(
            &ServerMessage::TaskLabelChanged {
                session_id,
                provider: AiProvider::CodexCode,
                task_label: String::from("finish the release"),
            },
            session_id,
            &live_sessions,
        )
        .await;
        let ai_state =
            AiProcessState::new_with_provider(AiProvider::CodexCode, AiState::WaitingForInput);
        persist_session_metadata(
            &ServerMessage::AiStateChanged { session_id, ai_state },
            session_id,
            &live_sessions,
        )
        .await;
        persist_session_metadata(
            &ServerMessage::PromptReceived {
                session_id,
                provider: AiProvider::CodexCode,
                text: String::from("finish the release"),
            },
            session_id,
            &live_sessions,
        )
        .await;

        {
            let mut sessions = live_sessions.write().await;
            dismiss_persisted_attention_state(sessions.get_mut(&session_id).unwrap());
        }
        persist_session_metadata(
            &ServerMessage::AiStateCleared { session_id },
            session_id,
            &live_sessions,
        )
        .await;

        let sessions = live_sessions.read().await;
        let retained = sessions.get(&session_id).unwrap();
        assert!(retained.ai_state.is_none());
        assert!(retained.ai_provider_hint.is_none());
        assert!(retained.prompt_state.is_none());
        assert!(retained.task_label.is_none());
    }

    #[tokio::test]
    async fn terminal_title_sources_persist_and_reset_independently() {
        let (server, _client) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server);
        let writer = test_shared_writer(server_write);
        let (session_id, live_sessions, _slaves) = live_session_with_sink(80, 24, &writer).await;

        persist_session_metadata(
            &ServerMessage::TitleChanged { session_id, title: String::from("editor") },
            session_id,
            &live_sessions,
        )
        .await;
        assert_eq!(
            live_sessions.read().await.get(&session_id).unwrap().title.as_deref(),
            Some("editor")
        );

        persist_session_metadata(
            &ServerMessage::IconTitleChanged { session_id, title: String::from("icon") },
            session_id,
            &live_sessions,
        )
        .await;
        persist_session_metadata(
            &ServerMessage::TitleChanged { session_id, title: String::from("newer window") },
            session_id,
            &live_sessions,
        )
        .await;
        {
            let sessions = live_sessions.read().await;
            let retained = sessions.get(&session_id).unwrap();
            assert_eq!(retained.title.as_deref(), Some("newer window"));
            assert_eq!(retained.icon_title.as_deref(), Some("icon"));
        }

        persist_session_metadata(
            &ServerMessage::IconTitleChanged { session_id, title: String::new() },
            session_id,
            &live_sessions,
        )
        .await;
        assert!(live_sessions.read().await.get(&session_id).unwrap().icon_title.is_none());

        persist_session_metadata(
            &ServerMessage::TitleChanged { session_id, title: String::new() },
            session_id,
            &live_sessions,
        )
        .await;
        assert!(live_sessions.read().await.get(&session_id).unwrap().title.is_none());
    }

    /// The grid a frame repaints the pane at, or `None` for any other frame.
    fn repainted_grid(msg: &ServerMessage) -> Option<(u16, u16)> {
        match msg {
            ServerMessage::ScreenSnapshot { snapshot, .. } => Some((snapshot.cols, snapshot.rows)),
            ServerMessage::SessionReplay { replay, .. } => Some((replay.cols, replay.rows)),
            _ => None,
        }
    }

    /// Read frames off `client_read` until one of them repaints the pane,
    /// returning the grid it repaints at. `None` once the socket is done.
    async fn next_repainted_grid<R>(client_read: &mut R) -> Option<(u16, u16)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        loop {
            let msg = read_message::<ServerMessage, _>(client_read).await.ok()?;
            if let Some(grid) = repainted_grid(&msg) {
                return Some(grid);
            }
        }
    }

    /// [`next_repainted_grid`] under a deadline, so a server that pushes nothing
    /// fails the assertion instead of hanging the suite.
    async fn await_screen_repair<R>(client_read: &mut R) -> Option<(u16, u16)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let deadline = std::time::Duration::from_secs(3);
        tokio::time::timeout(deadline, next_repainted_grid(client_read)).await.unwrap_or(None)
    }

    /// A drag's reports collapse into a leading apply plus a trailing one, and
    /// the client asks for the authoritative screen only when *it* changes size
    /// — so the trailing apply is the last thing that touches the grid and
    /// nobody asks about it afterwards. The server therefore owes the attached
    /// client a bounded whole-pane replay at the size the drag stopped on;
    /// without it the pane renders the pre-reflow grid until the next drag.
    // @lat: [[server#Sessions#Terminal Resize]]
    #[tokio::test]
    async fn a_coalesced_resize_burst_pushes_a_snapshot_at_the_size_it_settled_on() {
        let (server_sock, client_sock) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server_sock);
        let (mut client_read, _client_write) = tokio::io::split(client_sock);
        let writer = test_shared_writer(server_write);

        let (session_id, live_sessions, _slaves) = live_session_with_sink(120, 30, &writer).await;
        let attached: AttachedSessionIds =
            Arc::new(Mutex::new(std::iter::once(session_id).collect()));

        let report = |cols: u16| TerminalSize { cols, rows: 30, cell_width: 8, cell_height: 16 };
        // Leading apply, then two reports the pacer holds: only the newest of
        // them matures, as the size the drag stopped on.
        handle_resize(session_id, report(100), &live_sessions, &attached).await;
        handle_resize(session_id, report(90), &live_sessions, &attached).await;
        handle_resize(session_id, report(80), &live_sessions, &attached).await;

        let grid = await_screen_repair(&mut client_read).await;
        assert_eq!(
            grid,
            Some((80, 30)),
            "the trailing apply must hand the client the grid it landed on"
        );
    }

    /// The grid a session's `Term` currently holds, as (cols, rows).
    async fn live_term_grid(
        live_sessions: &LiveSessionRegistry,
        session_id: SessionId,
    ) -> (usize, usize) {
        use alacritty_terminal::grid::Dimensions as _;
        let term = {
            let sessions = live_sessions.read().await;
            Arc::clone(&sessions.get(&session_id).expect("session is still live").term)
        };
        let guard = term.lock().await;
        (guard.grid().columns(), guard.grid().screen_lines())
    }

    /// A trailing apply used to be gated on registry presence alone, so a drag
    /// that ended in a disconnect kept its armed timer: the next client could
    /// attach at a different geometry and watch the pre-detach size land on top
    /// of it up to an interval later. Detach now drops the size the timer is
    /// holding, and the attach-time resize tells the pacer it applied, so the
    /// stale timer finds nothing and the fresh grid stands.
    #[tokio::test]
    async fn a_stale_trailing_resize_never_lands_on_a_fresh_attach() {
        let (server_sock, _client_sock) = unix_stream_pair();
        let (_server_read, server_write) = tokio::io::split(server_sock);
        let writer = test_shared_writer(server_write);

        let (session_id, live_sessions, _slaves) = live_session_with_sink(120, 30, &writer).await;
        let attached: AttachedSessionIds =
            Arc::new(Mutex::new(std::iter::once(session_id).collect()));
        let report = |cols: u16| TerminalSize { cols, rows: 30, cell_width: 8, cell_height: 16 };

        // Mid-drag: a leading apply, then a report the pacer holds behind an
        // armed trailing timer.
        handle_resize(session_id, report(100), &live_sessions, &attached).await;
        handle_resize(session_id, report(90), &live_sessions, &attached).await;
        let armed_at = std::time::Instant::now();

        // The dragging client disconnects with that timer still counting down.
        let ids: HashSet<SessionId> = std::iter::once(session_id).collect();
        detach_sessions(&live_sessions, &ids, &writer).await;

        // A new client attaches at its own geometry, which the attach-time
        // resize drives straight into the `Term`.
        let (next_server_sock, _next_client_sock) = unix_stream_pair();
        let (_next_read, next_write) = tokio::io::split(next_server_sock);
        let next_writer = test_shared_writer(next_write);
        let next_attached: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));
        let reattached = crate::attach_flow::attach_sessions(
            &[session_id],
            &[report(140)],
            &live_sessions,
            crate::attach_flow::AttachClientContext {
                writer: &next_writer,
                attached_ids: &next_attached,
                additive: false,
            },
        )
        .await;
        assert!(reattached.contains(&session_id), "the session must come back attached");
        assert!(
            armed_at.elapsed() < RESIZE_APPLY_INTERVAL,
            "the detach and reattach have to fit inside the pacing interval, or the \
             timer this test is about has already matured on its own"
        );

        // The timer the departed drag armed finally matures.
        apply_trailing_resize(session_id, &live_sessions).await;

        assert_eq!(
            live_term_grid(&live_sessions, session_id).await,
            (140, 30),
            "a pre-detach report must not reflow the grid the new client attached at"
        );
    }

    #[tokio::test]
    async fn attach_sessions_returns_empty_when_registry_has_no_matching_sessions() {
        let live_sessions = new_live_session_registry();

        let (server, _client) = unix_stream_pair();
        let (_read, write) = tokio::io::split(server);
        let writer: SharedWriter = test_shared_writer(write);
        let attached_ids: AttachedSessionIds = Arc::new(Mutex::new(HashSet::new()));

        let attached = crate::attach_flow::attach_sessions(
            &[SessionId::new()],
            &[],
            &live_sessions,
            crate::attach_flow::AttachClientContext {
                writer: &writer,
                attached_ids: &attached_ids,
                additive: false,
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

    /// An unnamed `Hello` must land on the same window every time, and fan the
    /// rest out in the same order, however the set was built.
    // @lat: [[server#Workspaces#Window Assignment#Adoption order is stable]]
    #[test]
    fn adoption_order_does_not_depend_on_set_iteration() {
        let ids: Vec<WindowId> = (0..8).map(|_| WindowId::new()).collect();
        let connected: HashMap<WindowId, bool> = HashMap::new();
        let forwards: HashSet<WindowId> = ids.iter().copied().collect();
        let backwards: HashSet<WindowId> = ids.iter().rev().copied().collect();

        let (assigned, others) = resolve_window_assignment(None, &forwards, &connected);
        assert_eq!(
            resolve_window_assignment(None, &backwards, &connected),
            (assigned, others.clone())
        );

        // And the order is the id order, not an accident of insertion.
        let mut expected: Vec<WindowId> = ids;
        expected.sort_unstable_by_key(|wid| wid.to_full_string());
        assert_eq!(assigned, expected[0]);
        assert_eq!(others, expected[1..]);
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
        test_shared_writer(write)
    }

    /// One claim with its own writer — extracted so the concurrency test
    /// stays flat (no nested async block inside a loop).
    async fn claim_for(
        registry: WindowShares,
        all: HashSet<WindowId>,
        requested: WindowId,
    ) -> WindowId {
        let writer = test_writer();
        let (assigned, _) = claim_window(&registry, Some(requested), &all, &writer).await;
        assigned
    }

    /// Defect 1: a window already claimed by one connection must never be
    /// handed to a second connection, even when the claim+register is the
    /// only thing serialising them. Sequential form is deterministic. Now
    /// exercises the `WindowShare` registry and the preserved `Arc::ptr_eq`
    /// participant-identity invariant (feature 015 T009).
    #[tokio::test]
    async fn claim_window_rejects_already_claimed_window() {
        let w1 = WindowId::new();
        let all: HashSet<WindowId> = [w1].into_iter().collect();
        let registry = new_window_shares();

        let writer_a = test_writer();
        let (assigned_a, _) = claim_window(&registry, Some(w1), &all, &writer_a).await;
        assert_eq!(assigned_a, w1, "first client adopts the requested window");

        let writer_b = test_writer();
        let (assigned_b, _) = claim_window(&registry, Some(w1), &all, &writer_b).await;
        assert_ne!(assigned_b, w1, "second client must NOT get the same window");

        let shares = registry.read().await;
        assert!(
            shares.get(&w1).expect("w1 still owned").is_controlled_by(&writer_a),
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
        let registry = new_window_shares();

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
    /// `release_window_if_owned` path over the `WindowShare` registry, so the
    /// `Arc::ptr_eq` participant-identity guard is exercised end to end
    /// (feature 015 T009).
    #[tokio::test]
    async fn stale_detach_does_not_evict_new_owner() {
        let w1 = WindowId::new();
        let all: HashSet<WindowId> = [w1].into_iter().collect();
        let registry = new_window_shares();

        // Client A connects and adopts w1.
        let writer_a = test_writer();
        let (a_assigned, _) = claim_window(&registry, Some(w1), &all, &writer_a).await;
        assert_eq!(a_assigned, w1);

        // A disconnects cleanly — it owns w1, so the share is released.
        {
            let mut shares = registry.write().await;
            assert!(release_window_if_owned(&mut shares, w1, &writer_a));
            assert!(!shares.contains_key(&w1), "owner detach frees the window");
        }

        // Client B reconnects and legitimately re-adopts the now-free w1.
        let writer_b = test_writer();
        let (b_assigned, _) = claim_window(&registry, Some(w1), &all, &writer_b).await;
        assert_eq!(b_assigned, w1, "B adopts w1 once it is free");

        // A late/duplicate detach from A's old writer must NOT evict B.
        {
            let mut shares = registry.write().await;
            let now_empty = release_window_if_owned(&mut shares, w1, &writer_a);
            assert!(!now_empty, "registry not empty — B still owns w1");
            assert!(
                shares.get(&w1).expect("w1 retained").is_controlled_by(&writer_b),
                "stale detach from the old writer must not evict the new owner",
            );
        }

        // B's own detach releases it.
        {
            let mut shares = registry.write().await;
            assert!(release_window_if_owned(&mut shares, w1, &writer_b));
            assert!(!shares.contains_key(&w1), "owner detach removes the entry");
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
    fn ai_scrollback_preservation_covers_pi() {
        let pi = AiProcessState::new_with_provider(AiProvider::Pi, AiState::Processing);
        let events = [MetadataEvent::AiStateChanged(pi.clone())];

        assert!(ai_state_uses_ed3_filter(Some(&pi)));
        assert!(chunk_mentions_ed3_provider(&events));
        assert!(should_apply_ed3_filter(None, chunk_mentions_ed3_provider(&events)));
    }

    #[test]
    fn pi_never_triggers_the_claude_picker_filter() {
        assert!(ai_provider_uses_claude_picker_filter(Some(AiProvider::ClaudeCode)));
        assert!(!ai_provider_uses_claude_picker_filter(Some(AiProvider::CodexCode)));
        assert!(!ai_provider_uses_claude_picker_filter(Some(AiProvider::Pi)));
        assert!(!ai_provider_uses_claude_picker_filter(None));
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

    #[test]
    fn pi_task_label_uses_the_generic_wire_variant_not_codex_legacy() {
        let session_id = SessionId::new();
        let Some((changed_message, changed_cwd)) = convert_metadata_event(
            MetadataEvent::TaskLabelChanged {
                provider: AiProvider::Pi,
                label: String::from("fix the flaky test"),
            },
            session_id,
        ) else {
            panic!("convert_metadata_event returned None for TaskLabelChanged");
        };

        assert!(changed_cwd.is_none());
        assert!(matches!(
            changed_message,
            ServerMessage::TaskLabelChanged {
                session_id: sid,
                provider: AiProvider::Pi,
                task_label,
            } if sid == session_id && task_label == "fix the flaky test"
        ));

        let Some((cleared_message, cleared_cwd)) = convert_metadata_event(
            MetadataEvent::TaskLabelCleared { provider: AiProvider::Pi },
            session_id,
        ) else {
            panic!("convert_metadata_event returned None for TaskLabelCleared");
        };

        assert!(cleared_cwd.is_none());
        assert!(matches!(
            cleared_message,
            ServerMessage::TaskLabelCleared { session_id: sid, provider: AiProvider::Pi }
                if sid == session_id
        ));
    }

    #[tokio::test]
    async fn dispatch_action_routes_to_target_and_acknowledges_requester() {
        let window_shares = new_window_shares();
        let window_id = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter = test_shared_writer(request_write);

        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter = test_shared_writer(target_write);

        window_shares.write().await.insert(
            window_id,
            WindowShare::new_single_controller(Participant::local(&target_writer, false)),
        );

        handle_dispatch_action(
            Some(window_id),
            AutomationAction::OpenSettings,
            &window_shares,
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

    fn action_workspaces(assignments: &[(WindowId, SessionId)]) -> Arc<RwLock<WorkspaceManager>> {
        let mut manager = WorkspaceManager::new(Vec::new());
        let workspace_id = manager.create_workspace();
        for &(window_id, session_id) in assignments {
            manager.add_session(workspace_id, session_id, None);
            manager.assign_session_to_window(window_id, session_id);
        }
        Arc::new(RwLock::new(manager))
    }

    struct TestAgentActionContext {
        state: crate::agent_api::AgentApiState,
        window_shares: WindowShares,
        workspace_manager: Arc<RwLock<WorkspaceManager>>,
        caller: usize,
    }

    async fn run_test_agent_action(
        context: TestAgentActionContext,
        action: AutomationAction,
        origin_session_id: Option<SessionId>,
    ) -> Result<scribe_common::agent::AgentActionResult, scribe_common::agent::AgentError> {
        run_agent_action(
            AgentActionContext {
                state: &context.state,
                caller: context.caller,
                window_shares: &context.window_shares,
                workspace_manager: &context.workspace_manager,
            },
            None,
            action,
            origin_session_id,
        )
        .await
    }

    async fn read_correlated_action(
        client: &mut tokio::net::UnixStream,
    ) -> (u64, AutomationAction) {
        match read_message(client).await.unwrap() {
            ServerMessage::RunActionCorrelated { correlation_id, action } => {
                (correlation_id, action)
            }
            message => panic!("expected correlated action, got {message:?}"),
        }
    }

    // @lat: [[test#Test Harness#Agent API action activity]]
    #[tokio::test(start_paused = true)]
    async fn agent_action_activity_spans_correlated_completion_and_overlapping_leases() {
        let state = crate::agent_api::AgentApiState::new(scribe_common::config::AgentApiConfig {
            activity_dwell_ms: 37,
            ..scribe_common::config::AgentApiConfig::default()
        });
        let mut transitions = state.take_activity_transitions().unwrap();
        let window_shares = new_window_shares();
        let window_id = WindowId::new();
        let origin = SessionId::new();
        let workspaces = action_workspaces(&[(window_id, origin)]);
        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter = test_shared_writer(target_write);
        window_shares.write().await.insert(
            window_id,
            WindowShare::new_single_controller(Participant::local(&target_writer, false)),
        );
        let run = tokio::spawn(run_test_agent_action(
            TestAgentActionContext {
                state: state.clone(),
                window_shares: Arc::clone(&window_shares),
                workspace_manager: workspaces,
                caller: 41,
            },
            AutomationAction::NewTab,
            Some(origin),
        ));

        assert_eq!(transitions.recv().await, Some((origin, true)));
        let (correlation_id, action) = read_correlated_action(&mut target_client).await;
        assert!(matches!(action, AutomationAction::NewTab));
        assert!(!run.is_finished(), "activity must remain leased until completion");

        let overlap = state.activity().acquire(origin, 42);
        assert!(transitions.try_recv().is_err(), "overlap must not re-announce activity");
        let created_session_id = SessionId::new();
        assert!(state.complete_action(
            action_client_key(&target_writer),
            correlation_id,
            scribe_common::agent::AgentActionOutcome::Completed,
            Some(created_session_id),
        ));
        let result = run.await.unwrap().unwrap();
        assert_eq!(result.created_session_id, Some(created_session_id));

        tokio::time::advance(std::time::Duration::from_millis(37)).await;
        tokio::task::yield_now().await;
        assert!(transitions.try_recv().is_err(), "overlap keeps activity visible");
        drop(overlap);
        tokio::time::advance(std::time::Duration::from_millis(37)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(transitions.try_recv().ok(), Some((origin, false)));
    }

    // @lat: [[test#Test Harness#Agent API action activity]]
    #[tokio::test]
    async fn absent_stale_or_mismatched_action_origin_emits_no_activity() {
        let state = crate::agent_api::AgentApiState::default();
        let mut transitions = state.take_activity_transitions().unwrap();
        let window_shares = new_window_shares();
        let target_window = WindowId::new();
        let other_window = WindowId::new();
        let mismatched = SessionId::new();
        let stale_origin = SessionId::new();
        let workspaces = action_workspaces(&[(other_window, mismatched)]);
        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter = test_shared_writer(target_write);
        window_shares.write().await.insert(
            target_window,
            WindowShare::new_single_controller(Participant::local(&target_writer, false)),
        );

        for (caller, origin) in [(51, None), (52, Some(stale_origin)), (53, Some(mismatched))] {
            let run = tokio::spawn(run_test_agent_action(
                TestAgentActionContext {
                    state: state.clone(),
                    window_shares: Arc::clone(&window_shares),
                    workspace_manager: Arc::clone(&workspaces),
                    caller,
                },
                AutomationAction::OpenFind,
                origin,
            ));
            let (correlation_id, _) = read_correlated_action(&mut target_client).await;
            assert!(state.complete_action(
                action_client_key(&target_writer),
                correlation_id,
                scribe_common::agent::AgentActionOutcome::Completed,
                None,
            ));
            run.await.unwrap().unwrap();
            assert!(
                transitions.try_recv().is_err(),
                "absent, stale, or cross-window origin must not emit activity"
            );
        }
    }

    // @lat: [[test#Test Harness#Agent API action activity]]
    #[tokio::test]
    async fn focus_session_activity_uses_explicit_target_over_origin() {
        let state = crate::agent_api::AgentApiState::default();
        let mut transitions = state.take_activity_transitions().unwrap();
        let window_shares = new_window_shares();
        let target_window = WindowId::new();
        let other_window = WindowId::new();
        let focused = SessionId::new();
        let mismatched = SessionId::new();
        let workspaces = action_workspaces(&[(target_window, focused), (other_window, mismatched)]);
        let (target_server, mut target_client) = unix_stream_pair();
        let (_target_read, target_write) = tokio::io::split(target_server);
        let target_writer: SharedWriter = test_shared_writer(target_write);
        window_shares.write().await.insert(
            target_window,
            WindowShare::new_single_controller(Participant::local(&target_writer, false)),
        );
        let run = tokio::spawn(run_test_agent_action(
            TestAgentActionContext {
                state: state.clone(),
                window_shares: Arc::clone(&window_shares),
                workspace_manager: workspaces,
                caller: 61,
            },
            AutomationAction::FocusSession { session_id: focused },
            Some(mismatched),
        ));

        assert_eq!(transitions.recv().await, Some((focused, true)));
        let (correlation_id, _) = read_correlated_action(&mut target_client).await;
        assert!(state.complete_action(
            action_client_key(&target_writer),
            correlation_id,
            scribe_common::agent::AgentActionOutcome::Completed,
            None,
        ));
        run.await.unwrap().unwrap();
    }

    // @lat: [[test#Test Harness#Agent API action activity]]
    #[tokio::test]
    async fn agent_action_without_exactly_one_window_is_ambiguous_without_activity() {
        let state = crate::agent_api::AgentApiState::default();
        let mut transitions = state.take_activity_transitions().unwrap();
        let window_shares = new_window_shares();
        let origin = SessionId::new();
        let empty_workspaces = action_workspaces(&[]);
        assert!(matches!(
            run_agent_action(
                AgentActionContext {
                    state: &state,
                    caller: 1,
                    window_shares: &window_shares,
                    workspace_manager: &empty_workspaces,
                },
                None,
                AutomationAction::OpenFind,
                Some(origin),
            )
            .await,
            Err(scribe_common::agent::AgentError::AmbiguousTarget { .. })
        ));

        let mut assignments = Vec::new();
        for _ in 0..2 {
            let (server, _client) = unix_stream_pair();
            let (_read, write) = tokio::io::split(server);
            let writer = test_shared_writer(write);
            let window_id = WindowId::new();
            window_shares.write().await.insert(
                window_id,
                WindowShare::new_single_controller(Participant::local(&writer, false)),
            );
            assignments.push((window_id, origin));
        }
        let mapped_workspaces = action_workspaces(&assignments[..1]);
        assert!(matches!(
            run_agent_action(
                AgentActionContext {
                    state: &state,
                    caller: 1,
                    window_shares: &window_shares,
                    workspace_manager: &mapped_workspaces,
                },
                None,
                AutomationAction::OpenFind,
                Some(origin),
            )
            .await,
            Err(scribe_common::agent::AgentError::AmbiguousTarget { .. })
        ));
        assert!(transitions.try_recv().is_err(), "ambiguous actions must emit no activity");
    }

    #[tokio::test]
    async fn dispatch_action_reports_missing_window() {
        let window_shares = new_window_shares();
        let missing_window = WindowId::new();

        let (request_server, mut request_client) = unix_stream_pair();
        let (_request_read, request_write) = tokio::io::split(request_server);
        let request_writer: SharedWriter = test_shared_writer(request_write);

        handle_dispatch_action(
            Some(missing_window),
            AutomationAction::OpenSettings,
            &window_shares,
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

    /// Resolve a `Hello` claim against a map holding exactly `window` as a
    /// connected window, with no windows carrying sessions.
    fn resolve_claim_against_connected(
        requested: Option<WindowId>,
        window: WindowId,
        intent: ClaimIntent,
        controller: &ControllerIdentity,
        sharing_mode: scribe_config::SharingMode,
    ) -> ClaimResolution {
        // The value type is irrelevant to `resolve_window_claim` (it only asks
        // whether a window is connected), but a zero-sized one trips a lint, so
        // the map carries the window id again.
        let connected: HashMap<WindowId, WindowId> = HashMap::from([(window, window)]);
        let mode = claim_mode_for(intent, controller, sharing_mode);
        resolve_window_claim(requested, mode, &HashSet::new(), &connected)
    }

    // @lat: [[server#Remote Control#Sharing#Local Additive Join]]
    #[test]
    fn local_share_join_requires_explicit_intent() {
        let window = WindowId::new();

        assert!(matches!(
            resolve_claim_against_connected(
                Some(window),
                window,
                ClaimIntent::Plain,
                &ControllerIdentity::Local,
                scribe_config::SharingMode::SingleController,
            ),
            ClaimResolution::Assign { assigned, .. } if assigned != window
        ));
        for mode in
            [scribe_config::SharingMode::SharedSingleTypist, scribe_config::SharingMode::FreeForAll]
        {
            assert!(matches!(
                resolve_claim_against_connected(
                    Some(window),
                    window,
                    ClaimIntent::Plain,
                    &ControllerIdentity::Local,
                    mode,
                ),
                ClaimResolution::Assign { assigned, .. } if assigned != window
            ));
            assert!(
                matches!(
                    resolve_claim_against_connected(
                        Some(window),
                        window,
                        ClaimIntent::Join,
                        &ControllerIdentity::Local,
                        mode,
                    ),
                    ClaimResolution::AdditiveJoin { window_id } if window_id == window
                ),
                "{mode:?} must admit a local additive join"
            );
        }

        assert!(matches!(
            resolve_claim_against_connected(
                Some(window),
                window,
                ClaimIntent::Takeover,
                &ControllerIdentity::Local,
                scribe_config::SharingMode::FreeForAll,
            ),
            ClaimResolution::Takeover { .. }
        ));
        assert!(matches!(
            resolve_claim_against_connected(
                None,
                window,
                ClaimIntent::Plain,
                &ControllerIdentity::Local,
                scribe_config::SharingMode::FreeForAll,
            ),
            ClaimResolution::Assign { assigned, .. } if assigned != window
        ));
    }

    #[test]
    fn remote_claim_modes_are_unchanged() {
        let window = WindowId::new();
        let remote = ControllerIdentity::Remote {
            device_name: "peer".to_owned(),
            login_name: "someone".to_owned(),
        };

        assert!(matches!(
            resolve_claim_against_connected(
                Some(window),
                window,
                ClaimIntent::Plain,
                &remote,
                scribe_config::SharingMode::SingleController,
            ),
            ClaimResolution::LostControl { .. }
        ));
        for mode in
            [scribe_config::SharingMode::SharedSingleTypist, scribe_config::SharingMode::FreeForAll]
        {
            assert!(matches!(
                resolve_claim_against_connected(
                    Some(window),
                    window,
                    ClaimIntent::Plain,
                    &remote,
                    mode,
                ),
                ClaimResolution::AdditiveJoin { window_id } if window_id == window
            ));
        }
        assert!(matches!(
            resolve_claim_against_connected(
                Some(window),
                window,
                ClaimIntent::Takeover,
                &remote,
                scribe_config::SharingMode::FreeForAll,
            ),
            ClaimResolution::Takeover { window_id, .. } if window_id == window
        ));
    }

    // @lat: [[server#Sessions#PTY Reader Task]]
    #[tokio::test]
    async fn closed_clipboard_channel_parks_the_reader_arm() {
        let (tx, mut rx) = new_clipboard_command_channel();
        tx.send(ClipboardCommand::RefreshPolicy {
            policy: scribe_common::config::ClipboardPolicyConfig::default(),
        })
        .unwrap();
        drop(tx);

        // Buffered commands still arrive after the close.
        assert!(matches!(
            next_clipboard_command(&mut rx).await,
            ClipboardCommand::RefreshPolicy { .. }
        ));

        let drained = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            next_clipboard_command(&mut rx),
        )
        .await;
        assert!(
            drained.is_err(),
            "a drained, closed clipboard channel must park the select arm, not complete instantly"
        );
    }

    // @lat: [[server#Sessions#PTY Reader Task#Focus State On Reporting Enable]]
    #[test]
    fn focus_state_is_delivered_only_on_the_enable_edge() {
        let mut was_active = false;
        assert!(focus_mode_newly_enabled(&mut was_active, true), "off→on owes the state");
        assert!(!focus_mode_newly_enabled(&mut was_active, true), "steady on owes nothing");
        assert!(!focus_mode_newly_enabled(&mut was_active, false), "disable edge owes nothing");
        assert!(!focus_mode_newly_enabled(&mut was_active, false), "steady off owes nothing");
        assert!(focus_mode_newly_enabled(&mut was_active, true), "re-enable owes it again");
    }

    #[tokio::test]
    async fn finalizing_a_session_exit_cancels_its_reader() {
        let gate = SessionExitGate::new();
        let live_sessions = new_live_session_registry();
        let window_shares = new_window_shares();
        let workspace_manager = Arc::new(RwLock::new(WorkspaceManager::new(Vec::new())));
        let client_writer = initial_client_writer(None).await;
        let attachment: SessionAttachment = Arc::new(Mutex::new(None));

        finalize_session_exit(
            &gate,
            SessionExitContext {
                session_id: SessionId::new(),
                client_writer: &client_writer,
                attachment: &attachment,
                live_sessions: &live_sessions,
                window_shares: &window_shares,
                workspace_manager: &workspace_manager,
            },
            ChildExit::UNKNOWN,
        )
        .await;

        assert!(
            gate.is_cancelled(),
            "the exit funnel must stop the reader, including on the watcher path where nothing else does"
        );
    }

    /// Build a shared-mode share holding one local participant, the shape a
    /// viewport report needs (`SingleController` never reaches that path).
    async fn shared_window_with_participant(writer: &SharedWriter) -> (WindowShares, WindowId) {
        let window_shares = new_window_shares();
        let window_id = WindowId::new();
        window_shares.write().await.insert(
            window_id,
            WindowShare::new(
                Participant::local(writer, false),
                scribe_config::SharingMode::FreeForAll,
                scribe_config::ControlAcquisition::FreeClaim,
                None,
            ),
        );
        (window_shares, window_id)
    }

    fn viewport(cols: u16, rows: u16) -> TerminalSize {
        TerminalSize { cols, rows, cell_width: 8, cell_height: 16 }
    }

    /// Finding #24: a drag's worth of viewport reports used to arm one uncancelled
    /// 250 ms timer each, so every report drove its own apply. The burst must now
    /// arm exactly one timer, and reports landing while it sleeps must restart the
    /// window rather than schedule a second apply.
    #[tokio::test(start_paused = true)]
    async fn repeated_viewport_reports_settle_to_one_trailing_apply() {
        let writer = test_writer();
        let (window_shares, window_id) = shared_window_with_participant(&writer).await;

        let mut armed = Vec::new();
        for step in 0..12u16 {
            let report = viewport(120 - step, 40);
            armed.extend(record_viewport_report(&window_shares, window_id, &writer, report).await);
        }

        assert_eq!(armed.len(), 1, "a report burst arms exactly one trailing timer");
        let (debounce, generation) = armed[0];
        assert!(
            await_settled_viewport_reports(&window_shares, window_id, debounce, generation).await,
            "the armed timer waits out the burst and applies once"
        );

        let grid = &window_shares.read().await[&window_id].grid;
        assert!(!grid.apply_armed, "the settled timer disarms so a later burst can rearm");
        assert_eq!(grid.report_generation, 12, "every report is accounted for by the one timer");
    }

    /// A report that arrives after the previous burst settled must arm a fresh
    /// timer — the debounce coalesces a burst, it does not swallow later resizes.
    #[tokio::test(start_paused = true)]
    async fn a_report_after_the_burst_settles_arms_a_fresh_timer() {
        let writer = test_writer();
        let (window_shares, window_id) = shared_window_with_participant(&writer).await;

        let (debounce, generation) =
            record_viewport_report(&window_shares, window_id, &writer, viewport(100, 30))
                .await
                .expect("the first report arms a timer");
        assert!(
            await_settled_viewport_reports(&window_shares, window_id, debounce, generation).await
        );

        assert!(
            record_viewport_report(&window_shares, window_id, &writer, viewport(90, 30))
                .await
                .is_some(),
            "a later report arms its own trailing apply"
        );
    }

    /// A window closing mid-debounce must not drive a resize into a share that no
    /// longer exists; the waiter reports "nothing to apply" instead.
    #[tokio::test(start_paused = true)]
    async fn a_window_closing_mid_debounce_cancels_its_trailing_apply() {
        let writer = test_writer();
        let (window_shares, window_id) = shared_window_with_participant(&writer).await;

        let (debounce, generation) =
            record_viewport_report(&window_shares, window_id, &writer, viewport(100, 30))
                .await
                .expect("the first report arms a timer");
        window_shares.write().await.remove(&window_id);

        assert!(
            !await_settled_viewport_reports(&window_shares, window_id, debounce, generation).await,
            "a departed window has no grid to apply"
        );
    }

    /// Replay a report stream through a [`ResizePacer`], standing in for the
    /// trailing task: a timer armed at a deadline fires the moment the clock
    /// passes it, and one final drain settles the stream. Returns the instant of
    /// every apply and the size it carried.
    fn pace_reports(
        reports: &[(std::time::Instant, TerminalSize)],
    ) -> Vec<(std::time::Instant, TerminalSize)> {
        let mut pacer = ResizePacer::default();
        let mut applies = Vec::new();
        let mut deadline: Option<std::time::Instant> = None;
        for (at, report) in reports.iter().copied() {
            match deadline {
                Some(due) if due <= at => {
                    applies.extend(pacer.take_pending(due).map(|pending| (due, pending)));
                    deadline = None;
                }
                _ => {}
            }
            match pacer.admit(report, at) {
                ResizeAdmission::ApplyNow => applies.push((at, report)),
                ResizeAdmission::Arm(delay) => deadline = Some(at + delay),
                ResizeAdmission::Coalesced => {}
            }
        }
        if let Some(due) = deadline {
            applies.extend(pacer.take_pending(due).map(|pending| (due, pending)));
        }
        applies
    }

    /// Finding #25: a drag republished a pane's grid every frame and each report
    /// drove its own `Term` reflow plus `TIOCSWINSZ`, so the applies ran at event
    /// rate. The stream must now collapse to applies no closer than 250 ms apart,
    /// and the last one must carry the size the drag stopped at.
    #[test]
    fn a_resize_drag_paces_applies_to_four_per_second() {
        let base = std::time::Instant::now();
        let reports: Vec<_> = (0..60u16)
            .map(|step| {
                let at = base + std::time::Duration::from_millis(u64::from(step) * 16);
                (at, viewport(120 - step, 40))
            })
            .collect();

        let applies = pace_reports(&reports);

        assert!(
            applies.len() < reports.len() / 4,
            "60 reports over ~1s must not cost 60 reflows, got {}",
            applies.len()
        );
        for pair in applies.windows(2) {
            assert!(
                pair[1].0.duration_since(pair[0].0) >= RESIZE_APPLY_INTERVAL,
                "applies must stay at least one interval apart"
            );
        }
        assert_eq!(
            applies.last().expect("a drag applies at least once").1,
            reports.last().expect("the drag reported at least once").1,
            "the drag settles on the size it stopped at, not on a mid-drag one"
        );
    }

    /// Pacing must not tax an isolated resize: the first report of a stream, and
    /// any report a full interval after the previous apply, goes straight through.
    #[test]
    fn an_isolated_resize_applies_without_delay() {
        let base = std::time::Instant::now();
        let mut pacer = ResizePacer::default();

        assert!(
            matches!(pacer.admit(viewport(100, 30), base), ResizeAdmission::ApplyNow),
            "nothing has been applied yet, so the first report cannot be late"
        );
        let settled = base + RESIZE_APPLY_INTERVAL;
        assert!(
            matches!(pacer.admit(viewport(90, 30), settled), ResizeAdmission::ApplyNow),
            "a report a full interval later is a fresh resize, not part of a burst"
        );
        assert!(
            pacer.take_pending(settled).is_none(),
            "an immediate apply leaves no trailing work behind"
        );
    }
}
