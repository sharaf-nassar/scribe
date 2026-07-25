//! Inbound coalescing drain + outbound [`IpcSink`] bridging the frozen Scribe
//! IPC protocol to the GPUI terminal core.
//!
//! Inbound: server output events arrive on an mpsc channel and are drained with
//! Zed-style 4 ms / 100-event coalescing. Per-pane output is collapsed so each
//! batch runs one `write_output` and one repaint per dirty pane. The positional
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
//! `DismissUpdate`), and the window-lifecycle frames the close dialog, the
//! raise, the update decisions the status-bar CTA drives (`TriggerUpdate` /
//! `DismissUpdate`), the window-lifecycle frames the close dialog, the
//! window-list poll and the focus observer raise (`CloseWindow` / `QuitAll` /
//! `ListWindows` / `FocusChanged`), and the feature-014 LAN frames the approval
//! prompt and the startup LAN probe raise (`LanApprovalDecision` /
//! `ListLanPeers`), onto the ordered IPC-writer channel. The
//! outbound path never traverses the inbound drain, so keystrokes are never
//! queued behind an output firehose and `Resize` is always flushed ahead of the
//! `KeyInput` that follows.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use scribe_client_gpui::share::ControlIntent;
use scribe_common::{
    ids::{SessionId, WindowId, WorkspaceId},
    protocol::{ClientMessage, PromptMarkKind, TerminalSize, WorkspaceNotesMutation},
};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
    time::{Instant, timeout},
};

/// Maximum events folded into one drain batch before it is flushed regardless of
/// the time window. Mirrors Zed's terminal wakeup coalescing bound.
pub const MAX_BATCH_EVENTS: usize = 100;

/// Maximum wall-clock time a drain batch accumulates before it is flushed.
/// Mirrors Zed's 4 ms terminal wakeup coalescing window.
pub const BATCH_WINDOW: Duration = Duration::from_millis(4);

/// One inbound event handed to the drain. Every server message that mutates a
/// pane's terminal state is normalised before it enters the channel, so replay
/// decompression and snapshot-to-ANSI conversion happen off the drain and
/// coalescing only ever concatenates bytes.
///
/// The two non-output variants are here rather than applied straight from the
/// reader because both are *positional*: an OSC 133 mark names the row the
/// cursor is on and a suppressed-ED-3 `ScrollBottom` names the moment the
/// viewport must snap. The server emits each of them after the `PtyOutput`
/// chunk that moved the cursor, and the reader forwards messages in arrival
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

/// Drains `rx` with 4 ms / 100-event coalescing, invoking `apply` once per
/// batch with the per-pane-collapsed output. Returns when the channel closes,
/// flushing any final partial batch first.
///
/// `apply` runs synchronously between awaits, so a caller may lock its terminal
/// entities inside it without ever holding a lock across an await point.
pub async fn run_drain<F>(mut rx: UnboundedReceiver<InboundEvent>, mut apply: F)
where
    F: FnMut(CoalescedBatch),
{
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };
        let BatchWindow { batch, channel_closed } = collect_batch(first, &mut rx).await;
        apply(coalesce(batch));
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
pub async fn collect_batch(
    first: InboundEvent,
    rx: &mut UnboundedReceiver<InboundEvent>,
) -> BatchWindow {
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

    use tokio::sync::mpsc::unbounded_channel;

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
                        PaneOp::PromptMark { .. } | PaneOp::ScrollBottom => None,
                    })
                    .collect(),
            );
        }
    }

    fn flood(in_tx: &UnboundedSender<InboundEvent>, pane: SessionId, count: u32) {
        for _ in 0..count {
            in_tx.send(output(pane, b"A")).unwrap();
        }
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

        let (tx, rx) = unbounded_channel::<InboundEvent>();
        for event in events {
            tx.send(event).unwrap();
        }
        drop(tx);

        let recorded: RecordedBatches = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        run_drain(rx, move |batch| push_batch(&sink, batch)).await;

        let batches = recorded.lock().unwrap();
        // 300 events / 100-event cap = 3 batches; two panes each = at most 6 writes.
        let total_writes: usize = batches.iter().map(Vec::len).sum();
        assert!(total_writes <= 6, "expected coalescing, got {total_writes} write_output calls");
        assert_eq!(reconstruct(&batches, first), expected_first);
        assert_eq!(reconstruct(&batches, second), expected_second);
    }

    // @lat: [[test#GPUI IPC Bridge#Keystroke before output]]
    #[tokio::test]
    async fn keystroke_reaches_server_despite_inbound_firehose() {
        let pane = SessionId::new();
        let (in_tx, in_rx) = unbounded_channel::<InboundEvent>();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        for _ in 0..10_000u32 {
            in_tx.send(output(pane, b"A")).unwrap();
        }
        let drain = tokio::spawn(run_drain(in_rx, |_| {}));

        let sink = IpcSink::new(out_tx);
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
        let (in_tx, in_rx) = unbounded_channel::<InboundEvent>();
        let (out_tx, mut out_rx) = unbounded_channel::<ClientMessage>();
        let drain = tokio::spawn(run_drain(in_rx, |_| {}));
        let sink = IpcSink::new(out_tx);

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
