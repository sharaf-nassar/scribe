//! Inbound coalescing drain + outbound [`IpcSink`] bridging the frozen Scribe
//! IPC protocol to the GPUI terminal core.
//!
//! Inbound: server output events arrive on a bounded queue
//! ([`INBOUND_QUEUE_EVENTS`] events, [`INBOUND_QUEUE_BYTES`] of payload) and are
//! drained with Zed-style 4 ms / 100-event coalescing. Per-pane output is
//! collapsed so each batch runs one `write_output` and one repaint per dirty
//! pane. A queue that reaches either bound coalesces first and only then drops,
//! and the drain asks the server for a fresh screen per affected pane, so a
//! client that cannot keep up costs bounded memory instead of unbounded RSS.
//! The positional
//! events interleaved with that output — OSC 133 prompt marks and the
//! suppressed-ED-3 viewport snap — ride the same channel so they keep their
//! place in the pane's byte stream instead of racing it. Outbound:
//! [`IpcSink`] replaces Zed's `write_to_pty`, enqueuing `ClientMessage::KeyInput`,
//! `Resize`, the session-lifecycle messages the tab shortcuts drive
//! (`CreateSession` / `AttachSessions` / `Subscribe` / `RequestSnapshot` /
//! `CloseSession`), the feature-015
//! control-passing frames (`ControlClaim` / `ControlGrant`) the share surfaces
//! raise, the `SearchRequest` the find overlay issues on every query edit,
//! the update decisions the status-bar CTA drives (`TriggerUpdate` /
//! `DismissUpdate`), the window-lifecycle frames the close dialog, the
//! window-list poll and the focus observer raise (`CloseWindow` / `QuitAll` /
//! `ListWindows` / `FocusChanged`), the feature-014 LAN frames the approval
//! prompt and the startup LAN probe raise (`LanApprovalDecision` /
//! `ListLanPeers`), the workspace frames the window's split shell raises
//! (`CreateWorkspace` / `CloseWorkspace` / `MoveSession` /
//! `ReportWorkspaceTree`), and the feature-013 tailnet frames the startup
//! remote probe and the automation fallback raise (`ListRemotePeers` /
//! `DispatchAction`), onto the ordered IPC-writer channel. The
//! outbound path never traverses the inbound drain, so keystrokes are never
//! queued behind an output firehose and `Resize` is always flushed ahead of the
//! `KeyInput` that follows.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use scribe_client::share::ControlIntent;
use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{
        AutomationAction, ClientMessage, PromptMarkKind, TerminalSize, WorkspaceNotesMutation,
        WorkspaceTreeNode,
    },
};
use tokio::{
    sync::{Notify, mpsc::UnboundedSender},
    time::{Instant, timeout},
};

/// Maximum events folded into one drain batch before it is flushed regardless of
/// the time window. Mirrors Zed's terminal wakeup coalescing bound.
pub const MAX_BATCH_EVENTS: usize = 100;

/// Maximum wall-clock time a drain batch accumulates before it is flushed.
/// Mirrors Zed's 4 ms terminal wakeup coalescing window.
pub const BATCH_WINDOW: Duration = Duration::from_millis(4);

/// Maximum events the inbound queue buffers between the reader and the drain
/// before the overflow policy runs.
pub const INBOUND_QUEUE_EVENTS: usize = 256;

/// Maximum buffered [`InboundEvent::PaneOutput`] payload the inbound queue
/// holds.
///
/// The event bound alone does not bound memory: events carry variable-size
/// payloads, and coalescing deliberately trades event count for bytes, so
/// without this ceiling a firehose would collapse into a handful of ever-growing
/// buffers. One event larger than the whole ceiling is still admitted onto an
/// emptied queue — refusing the newest frame would stall the pane rather than
/// bound it — so the true high-water mark is this ceiling plus one frame.
pub const INBOUND_QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// How long an overflow resync waits for the drain to catch up before the
/// `RequestSnapshot` goes out anyway.
///
/// A resync taken while the queue is still full would be stale on arrival, so
/// the drain normally waits for an empty queue. A sustained firehose never
/// reaches empty, and a pane must not stay silently wrong for longer than this
/// while one lasts.
const RESYNC_MAX_DELAY: Duration = Duration::from_secs(2);

/// One inbound event handed to the drain. Every server message that mutates a
/// pane's terminal state is normalised before it enters the channel, so replay
/// decompression and snapshot-to-ANSI conversion happen off the drain and
/// coalescing only ever concatenates bytes.
///
/// The three non-output variants are here rather than applied straight from the
/// reader because all of them are *positional*: an OSC 133 mark names the row
/// the cursor is on, a suppressed-ED-3 `ScrollBottom` names the moment the
/// viewport must snap, and a `TrimScrollback` names a scrollback size measured
/// after a particular chunk. The server emits each of them after the
/// `PtyOutput` chunk they describe, and the reader forwards messages in arrival
/// order, so routing them down the same FIFO is what makes them land against a
/// grid that already holds the output they describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    /// Output bytes destined for the named pane's `write_output`.
    PaneOutput { session_id: SessionId, bytes: Vec<u8> },
    /// An OSC 133 prompt mark for the named pane. The wire message's
    /// `click_events` flag is dropped at the reader: click-to-move has no
    /// surface in this client, so carrying it here would be dead state.
    PromptMark { session_id: SessionId, kind: PromptMarkKind, exit_code: Option<i32> },
    /// The server suppressed an ED 3 for the named pane, so its viewport must
    /// snap to the live bottom the way a real ED 3 would have left it.
    ScrollBottom { session_id: SessionId },
    /// The server trimmed the named pane's scrollback back to `kept_rows`, so
    /// the display grid has to drop the same oldest rows.
    TrimScrollback { session_id: SessionId, kept_rows: usize },
}

/// One operation a drained batch applies to a pane, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOp {
    /// Concatenated output bytes for one `write_output`.
    Output(Vec<u8>),
    /// An OSC 133 mark to anchor against the grid as it stands after every
    /// preceding [`PaneOp::Output`] in this batch.
    PromptMark { kind: PromptMarkKind, exit_code: Option<i32> },
    /// Snap the pane's viewport to the live bottom.
    ScrollBottom,
    /// Drop the pane's oldest scrollback rows until `kept_rows` remain, and
    /// shift every surviving absolute-row anchor by however many went.
    TrimScrollback { kept_rows: usize },
}

/// A drained, per-pane-collapsed batch. A pane's consecutive output runs are
/// concatenated so applying the batch is one `write_output` per run, and the
/// positional events that interrupt a run keep their place in it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoalescedBatch {
    entries: Vec<(SessionId, PaneOp)>,
}

impl CoalescedBatch {
    /// Returns true when the batch touched no panes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the batch's operations in apply order.
    pub fn iter(&self) -> impl Iterator<Item = (SessionId, &PaneOp)> {
        self.entries.iter().map(|(id, op)| (*id, op))
    }
}

impl IntoIterator for CoalescedBatch {
    type Item = (SessionId, PaneOp);
    type IntoIter = std::vec::IntoIter<(SessionId, PaneOp)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

/// Collapses a drained run of events into per-pane operations.
///
/// Output for a pane is appended to that pane's open entry, so an all-output
/// batch still collapses to exactly one entry per pane in first-seen order. A
/// positional event closes the pane's open entry: output that arrives after a
/// prompt mark starts a fresh run, which is what keeps the mark anchored
/// between the bytes that preceded it and the bytes that followed.
#[must_use]
pub fn coalesce(events: impl IntoIterator<Item = InboundEvent>) -> CoalescedBatch {
    let mut open: HashMap<SessionId, usize> = HashMap::new();
    let mut entries: Vec<(SessionId, PaneOp)> = Vec::new();
    for event in events {
        match event {
            InboundEvent::PaneOutput { session_id, bytes } => {
                append_pane_output(&mut open, &mut entries, session_id, bytes);
            }
            InboundEvent::PromptMark { session_id, kind, exit_code } => {
                open.remove(&session_id);
                entries.push((session_id, PaneOp::PromptMark { kind, exit_code }));
            }
            InboundEvent::ScrollBottom { session_id } => {
                open.remove(&session_id);
                entries.push((session_id, PaneOp::ScrollBottom));
            }
            InboundEvent::TrimScrollback { session_id, kept_rows } => {
                open.remove(&session_id);
                entries.push((session_id, PaneOp::TrimScrollback { kept_rows }));
            }
        }
    }
    CoalescedBatch { entries }
}

/// Appends `bytes` to `session_id`'s open output entry, or starts a new one.
fn append_pane_output(
    open: &mut HashMap<SessionId, usize>,
    entries: &mut Vec<(SessionId, PaneOp)>,
    session_id: SessionId,
    bytes: Vec<u8>,
) {
    if let Some(&slot) = open.get(&session_id)
        && let Some((_, PaneOp::Output(buffered))) = entries.get_mut(slot)
    {
        buffered.extend_from_slice(&bytes);
        return;
    }
    open.insert(session_id, entries.len());
    entries.push((session_id, PaneOp::Output(bytes)));
}

/// Rebuilds the [`InboundEvent`] a coalesced entry came from. [`PaneOp`] and
/// `InboundEvent` carry the same information, so the queue can re-coalesce its
/// backlog through [`coalesce`] instead of growing a second ordering rule.
fn rehydrate((session_id, op): (SessionId, PaneOp)) -> InboundEvent {
    match op {
        PaneOp::Output(bytes) => InboundEvent::PaneOutput { session_id, bytes },
        PaneOp::PromptMark { kind, exit_code } => {
            InboundEvent::PromptMark { session_id, kind, exit_code }
        }
        PaneOp::ScrollBottom => InboundEvent::ScrollBottom { session_id },
        PaneOp::TrimScrollback { kept_rows } => {
            InboundEvent::TrimScrollback { session_id, kept_rows }
        }
    }
}

/// Payload an event contributes to the queue's byte budget.
fn payload_bytes(event: &InboundEvent) -> usize {
    match event {
        InboundEvent::PaneOutput { bytes, .. } => bytes.len(),
        InboundEvent::PromptMark { .. }
        | InboundEvent::ScrollBottom { .. }
        | InboundEvent::TrimScrollback { .. } => 0,
    }
}

/// The pane an event belongs to, which is also the pane a dropped event owes a
/// resync to.
fn event_session(event: &InboundEvent) -> SessionId {
    match event {
        InboundEvent::PaneOutput { session_id, .. }
        | InboundEvent::PromptMark { session_id, .. }
        | InboundEvent::ScrollBottom { session_id }
        | InboundEvent::TrimScrollback { session_id, .. } => *session_id,
    }
}

/// The bounded inbound queue's contents, plus the bookkeeping the overflow
/// policy runs on.
#[derive(Debug)]
struct InboundState {
    events: VecDeque<InboundEvent>,
    /// Buffered [`InboundEvent::PaneOutput`] payload, kept in step with
    /// `events` so the byte bound never needs a scan.
    bytes: usize,
    /// Panes whose queued events were dropped since the drain last looked.
    dropped: Vec<SessionId>,
    senders: usize,
    receiver_alive: bool,
    /// Set when a coalescing pass could not shrink the backlog, so the next push
    /// does not pay for a second full pass that cannot help either. Cleared once
    /// the drain has pulled the queue back down to half its bound.
    coalesce_stalled: bool,
}

impl InboundState {
    /// Queues `event`, applying the overflow policy: coalesce first, drop only
    /// what still does not fit, and record every dropped pane so the drain can
    /// resync it.
    fn admit(&mut self, event: InboundEvent) {
        let incoming = payload_bytes(&event);
        if self.events.len() >= INBOUND_QUEUE_EVENTS && !self.coalesce_stalled {
            let before = self.events.len();
            self.recoalesce();
            self.coalesce_stalled = self.events.len() >= before;
        }
        while self.events.len() >= INBOUND_QUEUE_EVENTS
            || self.bytes.saturating_add(incoming) > INBOUND_QUEUE_BYTES
        {
            let Some(evicted) = self.events.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(payload_bytes(&evicted));
            let session_id = event_session(&evicted);
            if !self.dropped.contains(&session_id) {
                self.dropped.push(session_id);
            }
        }
        self.bytes = self.bytes.saturating_add(incoming);
        self.events.push_back(event);
    }

    /// Collapses the backlog with exactly the rule the drain applies to a batch,
    /// so queue-level coalescing can never produce an order the drain would not
    /// have produced anyway. A pane's own events keep their order and every byte
    /// survives; only the event count falls.
    fn recoalesce(&mut self) {
        let pending = std::mem::take(&mut self.events);
        self.events = coalesce(pending).into_iter().map(rehydrate).collect();
    }

    /// Pops the oldest event for the drain.
    fn take(&mut self) -> Option<InboundEvent> {
        let event = self.events.pop_front()?;
        self.bytes = self.bytes.saturating_sub(payload_bytes(&event));
        if self.events.len() * 2 <= INBOUND_QUEUE_EVENTS {
            self.coalesce_stalled = false;
        }
        Some(event)
    }
}

/// Queue plus the wakeup its single drain task parks on.
#[derive(Debug)]
struct InboundShared {
    state: Mutex<InboundState>,
    notify: Notify,
}

/// Recovers a poisoned inbound lock rather than tearing the pane stream down:
/// every critical section here is a handful of field updates that cannot leave
/// the queue half-written, so the surviving state is still coherent.
fn lock_inbound(shared: &InboundShared) -> MutexGuard<'_, InboundState> {
    shared.state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The drain dropped its receiver, so no further inbound event can be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundClosed;

impl std::fmt::Display for InboundClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("inbound drain closed")
    }
}

/// Producer half of the bounded inbound queue, held by the IPC reader.
#[derive(Debug)]
pub struct InboundSender {
    shared: Arc<InboundShared>,
}

/// Consumer half of the bounded inbound queue, owned by the coalescing drain.
#[derive(Debug)]
pub struct InboundReceiver {
    shared: Arc<InboundShared>,
}

/// Creates the bounded inbound queue the reader feeds and [`run_drain`] owns.
#[must_use]
pub fn inbound_channel() -> (InboundSender, InboundReceiver) {
    let shared = Arc::new(InboundShared {
        state: Mutex::new(InboundState {
            events: VecDeque::new(),
            bytes: 0,
            dropped: Vec::new(),
            senders: 1,
            receiver_alive: true,
            coalesce_stalled: false,
        }),
        notify: Notify::new(),
    });
    (InboundSender { shared: Arc::clone(&shared) }, InboundReceiver { shared })
}

impl InboundSender {
    /// Queues one inbound event, never blocking the reader: a queue at either
    /// bound coalesces and then drops rather than back-pressuring the socket,
    /// because a stalled read would push the backlog onto the server's sink.
    ///
    /// # Errors
    /// Returns [`InboundClosed`] when the drain has dropped its receiver.
    pub fn send(&self, event: InboundEvent) -> Result<(), InboundClosed> {
        {
            let mut state = lock_inbound(&self.shared);
            if !state.receiver_alive {
                return Err(InboundClosed);
            }
            state.admit(event);
        }
        self.shared.notify.notify_one();
        Ok(())
    }
}

impl Clone for InboundSender {
    fn clone(&self) -> Self {
        lock_inbound(&self.shared).senders += 1;
        Self { shared: Arc::clone(&self.shared) }
    }
}

impl Drop for InboundSender {
    fn drop(&mut self) {
        let last = {
            let mut state = lock_inbound(&self.shared);
            state.senders = state.senders.saturating_sub(1);
            state.senders == 0
        };
        // `notify_one` stores a permit when no task is parked yet, so a receiver
        // between its emptiness check and its await still observes the close.
        if last {
            self.shared.notify.notify_one();
        }
    }
}

/// One non-blocking look at the inbound queue.
enum InboundPoll {
    Event(InboundEvent),
    Empty,
    Closed,
}

impl InboundReceiver {
    /// Waits for the next queued event, returning `None` once the queue is
    /// drained and every sender is gone.
    pub async fn recv(&mut self) -> Option<InboundEvent> {
        loop {
            match self.poll_once() {
                InboundPoll::Event(event) => return Some(event),
                InboundPoll::Closed => return None,
                InboundPoll::Empty => self.shared.notify.notified().await,
            }
        }
    }

    /// Takes the next event without waiting. Split out of [`Self::recv`] so the
    /// queue lock is released before the await rather than merely scoped away
    /// from it.
    fn poll_once(&self) -> InboundPoll {
        let mut state = lock_inbound(&self.shared);
        if let Some(event) = state.take() {
            return InboundPoll::Event(event);
        }
        if state.senders == 0 {
            return InboundPoll::Closed;
        }
        InboundPoll::Empty
    }

    /// Events still queued behind the drain.
    fn len(&self) -> usize {
        lock_inbound(&self.shared).events.len()
    }

    /// True once the drain has caught up with the reader.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes the panes whose events the overflow policy dropped since the last
    /// call.
    fn take_dropped(&mut self) -> Vec<SessionId> {
        std::mem::take(&mut lock_inbound(&self.shared).dropped)
    }
}

impl Drop for InboundReceiver {
    fn drop(&mut self) {
        let mut state = lock_inbound(&self.shared);
        state.receiver_alive = false;
        state.events.clear();
        state.bytes = 0;
    }
}

/// Overflow resyncs the drain still owes, one entry per pane.
#[derive(Debug, Default)]
struct PendingResync {
    sessions: Vec<SessionId>,
    since: Option<Instant>,
}

impl PendingResync {
    /// Folds in the panes the queue dropped during this batch, then asks the
    /// server for a fresh screen once the drain has caught up — or once the
    /// oldest debt is [`RESYNC_MAX_DELAY`] old, because a firehose that never
    /// lets the queue reach empty must still repair the pane.
    ///
    /// The request is per pane rather than per connection, which is what makes
    /// the policy correct for a shared session: each participant detects its own
    /// overflow and repaints only the panes it actually lost bytes for.
    fn settle(&mut self, rx: &mut InboundReceiver, sink: &IpcSink) {
        for session_id in rx.take_dropped() {
            if !self.sessions.contains(&session_id) {
                self.sessions.push(session_id);
            }
        }
        if self.sessions.is_empty() {
            return;
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if !rx.is_empty() && since.elapsed() < RESYNC_MAX_DELAY {
            return;
        }
        for session_id in self.sessions.drain(..) {
            match sink.request_snapshot(session_id) {
                Ok(()) => tracing::warn!(
                    %session_id,
                    "inbound queue overflowed; requested a screen snapshot resync"
                ),
                Err(error) => {
                    tracing::warn!(%session_id, %error, "inbound overflow resync request dropped");
                }
            }
        }
        self.since = None;
    }
}

/// Drains `rx` with 4 ms / 100-event coalescing, invoking `apply` once per
/// batch with the per-pane-collapsed output. Returns when the queue closes,
/// flushing any final partial batch first.
///
/// `apply` runs synchronously between awaits, so a caller may lock its terminal
/// entities inside it without ever holding a lock across an await point. After
/// each batch the drain settles whatever the bounded queue dropped, sending one
/// `RequestSnapshot` per affected pane through `sink`; because the request only
/// goes out once the queue is empty, the repaint it triggers lands on a calm
/// queue instead of being dropped in turn.
pub async fn run_drain<F>(mut rx: InboundReceiver, sink: IpcSink, mut apply: F)
where
    F: FnMut(CoalescedBatch),
{
    let mut resync = PendingResync::default();
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let BatchWindow { batch, channel_closed } = collect_batch(first, &mut rx).await;
        apply(coalesce(batch));
        resync.settle(&mut rx, &sink);
        if channel_closed {
            return;
        }
    }
}

/// A drained batch together with whether the channel closed while collecting it.
pub struct BatchWindow {
    /// Events folded into this batch, ready for [`coalesce`].
    pub batch: Vec<InboundEvent>,
    /// `true` when `rx` closed mid-collection, so the caller must stop after
    /// applying this final batch.
    pub channel_closed: bool,
}

/// Accumulates events into one 4 ms / 100-event batch window starting with
/// `first`, so the coalescing bound is shared between [`run_drain`] and the
/// synchronized-frame drain in `main.rs` rather than duplicated.
pub async fn collect_batch(first: InboundEvent, rx: &mut InboundReceiver) -> BatchWindow {
    let mut batch = Vec::with_capacity(MAX_BATCH_EVENTS);
    batch.push(first);
    let deadline = Instant::now() + BATCH_WINDOW;
    while batch.len() < MAX_BATCH_EVENTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => batch.push(event),
            Ok(None) => return BatchWindow { batch, channel_closed: true },
            Err(_) => break,
        }
    }
    BatchWindow { batch, channel_closed: false }
}

/// The outbound writer channel closed before a message could be enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkClosed;

impl std::fmt::Display for SinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IPC writer channel closed")
    }
}

/// One pane of a cold-restart snapshot, expressed as the session request that
/// re-creates it.
///
/// Grouped into a value rather than passed as five parallel arguments because
/// every field comes from the same persisted `LaunchRecord` and they are only
/// ever meaningful together.
#[derive(Debug, Clone)]
pub struct RestoredSession {
    /// The workspace region the restored pane belongs to.
    pub workspace_id: WorkspaceId,
    /// The grid the restored pane will occupy, so the PTY is spawned at the
    /// size it is about to be rendered at instead of the 80×24 default.
    pub size: TerminalSize,
    /// The directory the saved pane was working in.
    pub cwd: Option<PathBuf>,
    /// The program to spawn instead of a login shell (a custom command, or a
    /// provider resume for an AI pane).
    pub command: Option<Vec<String>>,
    /// The persisted `LaunchRecord.launch_id`, which the server looks the saved
    /// environment envelope up by.
    pub launch_id: String,
}

/// Outbound half of the bridge: the GPUI-side replacement for Zed's
/// `write_to_pty`. Enqueues `ClientMessage::KeyInput` / `Resize` onto the shared
/// IPC-writer channel, which is a single ordered FIFO drained by the writer
/// task. Because the sink is independent of the inbound drain, keystrokes are
/// never queued behind an output firehose; because the channel is ordered, a
/// `Resize` enqueued before a `KeyInput` reaches the server first.
#[derive(Debug, Clone)]
pub struct IpcSink {
    tx: UnboundedSender<ClientMessage>,
}

impl IpcSink {
    /// Wraps the IPC-writer channel sender.
    #[must_use]
    pub fn new(tx: UnboundedSender<ClientMessage>) -> Self {
        Self { tx }
    }

    /// Enqueues encoded key bytes for `session_id`.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn key_input(
        &self,
        session_id: SessionId,
        data: Vec<u8>,
        dismisses_attention: bool,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::KeyInput { session_id, data, dismisses_attention })
    }

    /// Enqueues a resize for `session_id`, ahead of any later `KeyInput`.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn resize(&self, session_id: SessionId, size: TerminalSize) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::Resize { session_id, size })
    }

    /// Announces that the client reapplied an on-disk config edit, so the
    /// server re-reads the same file and swaps its own live surfaces (clipboard
    /// policy, env store, remote/share listeners) in the same round trip.
    ///
    /// Emitted on every accepted reload, matching the legacy client's
    /// unconditional `finish_config_reload` send: the server decides for itself
    /// which of its surfaces actually changed.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn config_reloaded(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::ConfigReloaded)
    }

    /// Requests a new session (tab) in `workspace_id`.
    ///
    /// `command` spawns an explicit program instead of the login shell, which
    /// is how the AI-tab shortcuts open Claude Code / Codex; `cwd` inherits the
    /// active pane's directory. `split_direction` stays `None` because the tab
    /// shortcuts add to the existing workspace rather than dividing the window.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn create_session(
        &self,
        workspace_id: WorkspaceId,
        size: TerminalSize,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CreateSession {
            workspace_id,
            split_direction: None,
            cwd,
            size: Some(size),
            command,
            env_envelope_id: None,
        })
    }

    /// Requests the session that re-creates one pane of a cold-restart snapshot.
    ///
    /// Identical to [`Self::create_session`] except for `env_envelope_id`: the
    /// persisted `LaunchRecord.launch_id` the server looks the pane's saved
    /// environment envelope up by, so a relaunched shell comes back with the
    /// variables it had before the crash instead of a bare login environment.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn create_restored_session(&self, request: RestoredSession) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CreateSession {
            workspace_id: request.workspace_id,
            split_direction: None,
            cwd: request.cwd,
            size: Some(request.size),
            command: request.command,
            env_envelope_id: Some(request.launch_id),
        })
    }

    /// Attaches `session_ids` at `dimensions`, switching which sessions stream
    /// `PtyOutput`. Used when a tab shortcut changes the focused tab.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn attach_sessions(
        &self,
        session_ids: Vec<SessionId>,
        dimensions: Vec<TerminalSize>,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::AttachSessions { session_ids, dimensions })
    }

    /// Subscribes to `session_ids` immediately after the `AttachSessions` that
    /// made them visible.
    ///
    /// The server only honours a subscription for a session this connection is
    /// already attached to, so this must stay on the same ordered channel and
    /// behind the attach — the writer FIFO guarantees both. Subscribing makes
    /// the server run its CWD-fallback check for the newly visible panes, which
    /// is how a reattached tab gets its working directory (and the workspace
    /// name derived from it) without waiting for the next shell prompt.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn subscribe(&self, session_ids: Vec<SessionId>) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::Subscribe { session_ids })
    }

    /// Asks the server for `session_id`'s authoritative per-cell screen.
    ///
    /// This client is display-only: it owns no PTY and cannot re-derive a
    /// pane's grid locally, so whenever the pane may have drifted from the
    /// server's `Term` — after a cell-metric change raises `SIGWINCH` on the
    /// PTY, or
    /// after a reattach replay failed to decode — the only way back to a
    /// correct pane is to ask for the server's current state. The reply is a
    /// `ScreenSnapshot` carrying the visible grid *and* the scrollback, which
    /// the reader applies through `session_lifecycle::snapshot_reset_bytes`.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn request_snapshot(&self, session_id: SessionId) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::RequestSnapshot { session_id })
    }

    /// Asks the server to terminate `session_id`, backing the `close_tab`
    /// shortcut. The tab leaves the strip once `SessionExited` arrives.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn close_session(&self, session_id: SessionId) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CloseSession { session_id })
    }

    /// Asks the server to find `query` in `session_id`'s screen and scrollback,
    /// answered with a single `SearchResults` carrying up to `limit` spans.
    ///
    /// The search runs server-side for the same reason the snapshot does: this
    /// client is display-only and holds only the visible viewport, so it cannot
    /// match against the scrollback the user is actually searching. Sent on
    /// every edit of the find overlay's query, which is why the reply carries
    /// the query back — a stale answer is dropped rather than shown.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn search_request(
        &self,
        session_id: SessionId,
        query: String,
        limit: u32,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::SearchRequest { session_id, query, limit })
    }

    /// Requests the authoritative workspace notes for `workspace_ids` so the
    /// modal and hover preview can render server-owned state.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn workspace_notes_get(&self, workspace_ids: Vec<WorkspaceId>) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::WorkspaceNotesGet { workspace_ids })
    }

    /// Asks the server to mint a workspace for the window region the
    /// `workspace_split_*` chords just opened.
    ///
    /// A workspace region is a *server* concept — it owns the accent colour,
    /// the auto-derived name and the notes collection — so a region the client
    /// invents locally is a layout box with nothing behind it. The reply is a
    /// single `WorkspaceInfo` carrying the real [`WorkspaceId`], which the shell
    /// adopts onto the region that asked (see `PaneShell::adopt_pending_workspace`).
    /// No id is sent because only the server may allocate one.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn create_workspace(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CreateWorkspace)
    }

    /// Tells the server that `workspace_id`'s region collapsed, so it can drop
    /// the workspace the client no longer shows.
    ///
    /// Sent only for a region the server actually minted: a region still
    /// waiting for its `WorkspaceInfo` names an id the server has never seen,
    /// and closing it would be a lie about state that never existed.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn close_workspace(&self, workspace_id: WorkspaceId) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CloseWorkspace { workspace_id })
    }

    /// Reports that `session_id` now lives in `target_workspace`.
    ///
    /// A session and a region are independent axes: a split seeds its session
    /// through the workspace the tab strip was pointing at, and the pane that
    /// adopts it may belong to a different region entirely. This is the frame
    /// that reconciles the two, so the server's session→workspace map matches
    /// what the window shows.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn move_session(
        &self,
        session_id: SessionId,
        target_workspace: WorkspaceId,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::MoveSession { session_id, target_workspace })
    }

    /// Publishes the window's current workspace/pane split tree so the server
    /// can persist it for reconnect and handoff.
    ///
    /// Sent after every tree mutation — a pane split or close, a workspace
    /// split or collapse, and the session adoption that fills a fresh pane —
    /// because the server stores the last tree it was told about and replays it
    /// in `SessionList`. A client that never reports leaves that store empty and
    /// every reconnect rebuilds a single flat pane.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn report_workspace_tree(&self, tree: WorkspaceTreeNode) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::ReportWorkspaceTree { tree })
    }

    /// Sends a feature-015 control-passing intent for a shared window: the
    /// viewer's take-control affordance lowers to `ControlClaim`, and the
    /// grant/deny prompt's answer lowers to `ControlGrant`.
    ///
    /// The intent is lowered here rather than in the view so the shell never
    /// hand-builds a v3 control frame; the mapping lives once in
    /// [`ControlIntent::into_message`].
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn control_intent(&self, intent: ControlIntent) -> Result<(), SinkClosed> {
        self.enqueue(intent.into_message())
    }

    /// Requests a server-side workspace-notes mutation (draft save, note
    /// create/edit, archive, or bulk archive edit) built by the modal or
    /// preview from the current editor state.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn workspace_notes_mutate(
        &self,
        mutation: WorkspaceNotesMutation,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::WorkspaceNotesMutate { mutation })
    }

    /// Confirms the pending update, so the server downloads, verifies, and
    /// installs it and reports each step back as `UpdateProgress`. Sent when the
    /// user picks "Update Now" in the centred status-bar CTA's confirmation.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn trigger_update(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::TriggerUpdate)
    }

    /// Dismisses the pending update, so the server stops re-notifying about
    /// this version. Sent when the user picks "Later" (or presses Esc) in the
    /// centred status-bar CTA's confirmation.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn dismiss_update(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::DismissUpdate)
    }

    /// Asks the server to destroy `window_id` and every session it owns, which
    /// is the close dialog's "Kill Window". The server answers `WindowClosed`
    /// and the shell exits on that acknowledgement, never on this send.
    ///
    /// The id is the one `Welcome` handed this connection: the server refuses a
    /// `CloseWindow` naming any other window.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn close_window(&self, window_id: WindowId) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CloseWindow { window_id })
    }

    /// Asks the server to bring every window down gracefully, which is the
    /// close dialog's "Quit Scribe". The server answers every connected client
    /// — this one included — with `QuitRequested`; sessions are preserved.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn quit_all(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::QuitAll)
    }

    /// Polls the server for the windows it knows about, answered with a single
    /// `WindowList`. Drives the status bar's owning-machine remote-control
    /// summary, so it is only sent while `remote.enabled` makes that summary
    /// meaningful.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn list_windows(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::ListWindows)
    }

    /// Answers a pending feature-014 LAN device approval with the owning user's
    /// decision, echoing the `request_id` the server's `LanApprovalRequest`
    /// correlated the held connection by.
    ///
    /// `approve` writes a `TrustedDevice` and lets the peer attach; `false`
    /// refuses and reveals nothing. This is deliberately the ONLY way a decision
    /// leaves the client: the server ignores a decision arriving over any remote
    /// transport, so it must ride this local session connection.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn lan_approval_decision(&self, request_id: u64, approve: bool) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::LanApprovalDecision { request_id, approve })
    }

    /// Asks the local server which LAN peers it has discovered over mDNS on the
    /// current network, answered with a single `LanPeerList`.
    ///
    /// Served from THIS machine's own discovery view and refused over any remote
    /// transport, so it is only sent while the client is on its local socket and
    /// `remote.lan.enabled` makes the answer meaningful.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn list_lan_peers(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::ListLanPeers)
    }

    /// Answers a spec-010 OSC 52 message the server sent this client.
    ///
    /// The two answers — `ClipboardPromptResponse` for a confirmation overlay
    /// and `ClipboardBridgeReadReply` for a host clipboard read — are built by
    /// [`scribe_client::clipboard`] from the parked request, because only
    /// that module knows how a `BridgeError` collapses onto the reply. This
    /// method is the seam that puts the finished frame on the ordered writer
    /// channel; anything else is refused rather than sent, so the clipboard
    /// path cannot become a generic escape hatch onto the wire.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn clipboard_answer(&self, message: ClientMessage) -> Result<(), SinkClosed> {
        debug_assert!(
            matches!(
                message,
                ClientMessage::ClipboardPromptResponse { .. }
                    | ClientMessage::ClipboardBridgeReadReply { .. }
            ),
            "clipboard_answer takes only the two spec-010 client answers",
        );
        self.enqueue(message)
    }

    /// Asks the local server which same-account tailnet peers are online,
    /// answered with a single `RemotePeerList` (feature 013).
    ///
    /// Served from THIS machine's own `LocalAPI` view and refused over any
    /// remote transport, so it is only sent while the client is on its local
    /// socket and `remote.enabled` makes the answer meaningful.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn list_remote_peers(&self) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::ListRemotePeers)
    }

    /// Asks the server to route one automation action to the window's registered
    /// controller, answered with a single `ActionDispatched` naming the window it
    /// reached (feature 013 automation).
    ///
    /// `window_id: None` means "the window this connection registered"; the
    /// server refuses any other id from a registered connection, so that is the
    /// only value the client ever sends. The controller the server forwards the
    /// resulting `RunAction` to is not necessarily this process: under a share or
    /// a remote takeover it is whichever client currently drives the window,
    /// which is exactly why an action this shell cannot run locally is offered to
    /// it rather than dropped.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn dispatch_action(
        &self,
        window_id: Option<WindowId>,
        action: AutomationAction,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::DispatchAction { window_id, action })
    }

    /// Reports a pane focus transition so the server can relay CSI focus events
    /// (`\x1b[I` / `\x1b[O`) to PTY applications that enabled DECSET 1004.
    ///
    /// Sent for a window activation change and for a tab switch alike: both
    /// collapse to the same gained/lost pair before they reach the sink.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn focus_changed(
        &self,
        gained: Option<SessionId>,
        lost: Option<SessionId>,
    ) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::FocusChanged { gained, lost })
    }

    fn enqueue(&self, message: ClientMessage) -> Result<(), SinkClosed> {
        self.tx.send(message).map_err(|_| SinkClosed)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Instant,
    };

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;

    /// Recorded per-batch `write_output` calls captured by the drain test.
    type RecordedBatches = Arc<Mutex<Vec<Vec<(SessionId, Vec<u8>)>>>>;

    fn push_batch(recorded: &RecordedBatches, batch: CoalescedBatch) {
        if let Ok(mut guard) = recorded.lock() {
            guard.push(
                batch
                    .into_iter()
                    .filter_map(|(id, op)| match op {
                        PaneOp::Output(bytes) => Some((id, bytes)),
                        PaneOp::PromptMark { .. }
                        | PaneOp::ScrollBottom
                        | PaneOp::TrimScrollback { .. } => None,
                    })
                    .collect(),
            );
        }
    }

    fn flood(in_tx: &InboundSender, pane: SessionId, count: u32) {
        for _ in 0..count {
            in_tx.send(output(pane, b"A")).unwrap();
        }
    }

    /// A sink whose receiver is kept alive, for drains whose resync path is not
    /// under test.
    fn idle_sink() -> (IpcSink, UnboundedReceiver<ClientMessage>) {
        let (out_tx, out_rx) = unbounded_channel::<ClientMessage>();
        (IpcSink::new(out_tx), out_rx)
    }

    fn output(session_id: SessionId, bytes: &[u8]) -> InboundEvent {
        InboundEvent::PaneOutput { session_id, bytes: bytes.to_vec() }
    }

    fn sample_size() -> TerminalSize {
        TerminalSize { cols: 80, rows: 24, cell_width: 8, cell_height: 16 }
    }

    /// Builds a strictly-alternating two-pane firehose plus the expected
    /// per-pane byte streams.
    fn firehose(
        count: u32,
        first: SessionId,
        second: SessionId,
    ) -> (Vec<InboundEvent>, Vec<u8>, Vec<u8>) {
        let mut events = Vec::new();
        let mut expected_first = Vec::new();
        let mut expected_second = Vec::new();
        for index in 0..count {
            let byte: u8 = (index % 251).try_into().unwrap();
            let even = index % 2 == 0;
            let pane = if even { first } else { second };
            events.push(output(pane, &[byte]));
            if even {
                expected_first.push(byte);
            } else {
                expected_second.push(byte);
            }
        }
        (events, expected_first, expected_second)
    }

    /// Concatenates one pane's bytes across all recorded batches in order.
    fn reconstruct(batches: &[Vec<(SessionId, Vec<u8>)>], pane: SessionId) -> Vec<u8> {
        batches
            .iter()
            .flatten()
            .filter(|(id, _)| *id == pane)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect()
    }

    fn assert_key_input(message: &ClientMessage, expected: &[u8]) {
        match message {
            ClientMessage::KeyInput { data, .. } => assert_eq!(data.as_slice(), expected),
            other => panic!("expected KeyInput, got {other:?}"),
        }
    }

    // @lat: [[test#GPUI IPC Bridge#Coalesce collapses per pane]]
    #[test]
    fn coalesce_collapses_per_pane_preserving_first_seen_order() {
        let first = SessionId::new();
        let second = SessionId::new();
        let batch = coalesce([
            output(first, b"1"),
            output(second, b"x"),
            output(first, b"2"),
            output(second, b"y"),
            output(first, b"3"),
        ]);

        assert!(!batch.is_empty());
        assert!(coalesce(std::iter::empty()).is_empty());
        assert_eq!(
            batch.into_iter().collect::<Vec<_>>(),
            vec![
                (first, PaneOp::Output(b"123".to_vec())),
                (second, PaneOp::Output(b"xy".to_vec())),
            ]
        );
    }

    // @lat: [[test#GPUI IPC Bridge#Prompt marks split a pane's output run]]
    #[test]
    fn coalesce_keeps_positional_events_between_output_runs() {
        let pane = SessionId::new();
        let other = SessionId::new();
        let batch = coalesce([
            output(pane, b"before"),
            output(other, b"bg"),
            InboundEvent::PromptMark {
                session_id: pane,
                kind: PromptMarkKind::PromptStart,
                exit_code: None,
            },
            output(pane, b"after"),
            InboundEvent::ScrollBottom { session_id: pane },
            output(pane, b"tail"),
        ]);

        assert_eq!(
            batch.into_iter().collect::<Vec<_>>(),
            vec![
                (pane, PaneOp::Output(b"before".to_vec())),
                (other, PaneOp::Output(b"bg".to_vec())),
                (pane, PaneOp::PromptMark { kind: PromptMarkKind::PromptStart, exit_code: None }),
                (pane, PaneOp::Output(b"after".to_vec())),
                (pane, PaneOp::ScrollBottom),
                (pane, PaneOp::Output(b"tail".to_vec())),
            ]
        );
    }

    // @lat: [[test#GPUI IPC Bridge#Drain coalesces firehose]]
    #[tokio::test]
    async fn drain_coalesces_firehose_into_few_per_pane_batches() {
        let first = SessionId::new();
        let second = SessionId::new();
        let (events, expected_first, expected_second) = firehose(300, first, second);

        let (tx, rx) = inbound_channel();
        for event in events {
            tx.send(event).unwrap();
        }
        drop(tx);

        let recorded: RecordedBatches = Arc::new(Mutex::new(Vec::new()));
        let collector = Arc::clone(&recorded);
        let (sink, _out_rx) = idle_sink();
        run_drain(rx, sink, move |batch| push_batch(&collector, batch)).await;

        let batches = recorded.lock().unwrap();
        // 300 events / 100-event cap = 3 batches; two panes each = at most 6 writes.
        let total_writes: usize = batches.iter().map(Vec::len).sum();
        assert!(total_writes <= 6, "expected coalescing, got {total_writes} write_output calls");
        assert_eq!(reconstruct(&batches, first), expected_first);
        assert_eq!(reconstruct(&batches, second), expected_second);
    }

    /// One server frame's worth of output, sized so the byte ceiling is the
    /// bound a firehose reaches first.
    fn frame() -> Vec<u8> {
        vec![b'A'; 64 * 1024]
    }

    // @lat: [[test#GPUI IPC Bridge#Inbound queue bounds a firehose]]
    #[test]
    fn inbound_queue_holds_a_firehose_inside_its_event_and_byte_ceilings() {
        let pane = SessionId::new();
        let (in_tx, in_rx) = inbound_channel();

        // Thirty-two times the byte ceiling with no drain running at all.
        for _ in 0..2_000u32 {
            in_tx.send(output(pane, &frame())).unwrap();
        }

        let state = lock_inbound(&in_rx.shared);
        assert!(
            state.events.len() <= INBOUND_QUEUE_EVENTS,
            "queued {} events, bound is {INBOUND_QUEUE_EVENTS}",
            state.events.len()
        );
        assert!(
            state.bytes <= INBOUND_QUEUE_BYTES,
            "queued {} bytes, bound is {INBOUND_QUEUE_BYTES}",
            state.bytes
        );
        assert_eq!(state.dropped, vec![pane], "overflow must record the pane it dropped");
    }

    // @lat: [[test#GPUI IPC Bridge#Overflow resyncs the dropped pane]]
    #[tokio::test]
    async fn inbound_overflow_requests_a_snapshot_for_the_dropped_pane() {
        let pane = SessionId::new();
        let (in_tx, in_rx) = inbound_channel();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let sink = IpcSink::new(out_tx);

        let frames = INBOUND_QUEUE_BYTES / frame().len() + 8;
        for _ in 0..frames {
            in_tx.send(output(pane, &frame())).unwrap();
        }
        drop(in_tx);

        run_drain(in_rx, sink, |_| {}).await;

        match out_rx.recv().await.unwrap() {
            ClientMessage::RequestSnapshot { session_id } => assert_eq!(session_id, pane),
            other => panic!("expected RequestSnapshot, got {other:?}"),
        }
        assert!(out_rx.try_recv().is_err(), "one resync per overflowed pane, not one per drop");
    }

    // @lat: [[test#GPUI IPC Bridge#Keystroke before output]]
    #[tokio::test]
    async fn keystroke_reaches_server_despite_inbound_firehose() {
        let pane = SessionId::new();
        let (in_tx, in_rx) = inbound_channel();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        for _ in 0..10_000u32 {
            in_tx.send(output(pane, b"A")).unwrap();
        }
        let sink = IpcSink::new(out_tx);
        let drain = tokio::spawn(run_drain(in_rx, sink.clone(), |_| {}));
        sink.key_input(pane, b"x".to_vec(), true).unwrap();

        let message =
            tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await.unwrap().unwrap();
        assert_key_input(&message, b"x");

        drop(in_tx);
        drain.await.unwrap();
    }

    // @lat: [[test#GPUI IPC Bridge#Typing under firehose]]
    #[tokio::test]
    async fn typing_under_firehose_preserves_order_without_latency_spike() {
        let pane = SessionId::new();
        let (in_tx, in_rx) = inbound_channel();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let sink = IpcSink::new(out_tx);
        let drain = tokio::spawn(run_drain(in_rx, sink.clone(), |_| {}));

        for &key in b"echo hello\r" {
            flood(&in_tx, pane, 500);
            let start = Instant::now();
            sink.key_input(pane, vec![key], true).unwrap();
            let message = tokio::time::timeout(Duration::from_millis(100), out_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(start.elapsed() < Duration::from_millis(100), "keystroke latency spike");
            assert_key_input(&message, &[key]);
        }

        drop(in_tx);
        drain.await.unwrap();
    }

    // @lat: [[test#GPUI IPC Bridge#Resize before key input]]
    #[tokio::test]
    async fn resize_is_flushed_before_following_key_input() {
        let pane = SessionId::new();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let sink = IpcSink::new(out_tx);

        sink.resize(pane, sample_size()).unwrap();
        sink.key_input(pane, b"a".to_vec(), true).unwrap();

        assert!(matches!(out_rx.recv().await.unwrap(), ClientMessage::Resize { .. }));
        assert!(matches!(out_rx.recv().await.unwrap(), ClientMessage::KeyInput { .. }));
    }

    // @lat: [[test#GPUI Client Headless Suites#Config live reload#Reload announces ConfigReloaded]]
    #[tokio::test]
    async fn config_reloaded_is_enqueued_on_the_ordered_writer_channel() {
        let pane = SessionId::new();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let sink = IpcSink::new(out_tx);

        sink.config_reloaded().unwrap();
        // Ordering matters: the server must have re-read the config before it
        // interprets whatever the user types next.
        sink.key_input(pane, b"a".to_vec(), true).unwrap();

        assert!(matches!(out_rx.recv().await.unwrap(), ClientMessage::ConfigReloaded));
        assert!(matches!(out_rx.recv().await.unwrap(), ClientMessage::KeyInput { .. }));
    }

    // @lat: [[test#GPUI Client Headless Suites#Find overlay#The overlay's query reaches the wire]]
    #[tokio::test]
    async fn search_request_carries_the_query_and_the_result_limit() {
        let pane = SessionId::new();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let sink = IpcSink::new(out_tx);

        sink.search_request(pane, "error".to_owned(), 256).unwrap();

        match out_rx.recv().await.unwrap() {
            ClientMessage::SearchRequest { session_id, query, limit } => {
                assert_eq!(session_id, pane);
                assert_eq!(query, "error");
                assert_eq!(limit, 256);
            }
            other => panic!("expected SearchRequest, got {other:?}"),
        }
    }

    // @lat: [[test#GPUI IPC Bridge#Sink reports closed writer]]
    #[test]
    fn key_input_errors_when_writer_dropped() {
        let (out_tx, out_rx) = unbounded_channel::<ClientMessage>();
        drop(out_rx);
        let sink = IpcSink::new(out_tx);
        assert_eq!(sink.key_input(SessionId::new(), b"a".to_vec(), false), Err(SinkClosed));
    }
}
