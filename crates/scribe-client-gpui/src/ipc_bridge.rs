//! Inbound coalescing drain + outbound [`IpcSink`] bridging the frozen Scribe
//! IPC protocol to the GPUI terminal core.
//!
//! Inbound: server output events arrive on an mpsc channel and are drained with
//! Zed-style 4 ms / 100-event coalescing. Per-pane output is collapsed so each
//! batch runs one `write_output` and one repaint per dirty pane. Outbound:
//! [`IpcSink`] replaces Zed's `write_to_pty`, enqueuing `ClientMessage::KeyInput`,
//! `Resize`, the session-lifecycle messages the tab shortcuts drive
//! (`CreateSession` / `AttachSessions` / `CloseSession`), and the feature-015
//! control-passing frames (`ControlClaim` / `ControlGrant`) the share surfaces
//! raise, onto the ordered IPC-writer channel. The outbound path never
//! traverses the inbound drain, so keystrokes are never queued behind an output
//! firehose and `Resize` is always flushed ahead of the `KeyInput` that follows.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use scribe_client_gpui::share::ControlIntent;
use scribe_common::{
    ids::{SessionId, WorkspaceId},
    protocol::{ClientMessage, TerminalSize, WorkspaceNotesMutation},
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
/// pane's terminal state is normalised to raw output bytes before it enters the
/// channel, so replay decompression and snapshot-to-ANSI conversion happen off
/// the drain and coalescing only ever concatenates bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    /// Output bytes destined for the named pane's `write_output`.
    PaneOutput { session_id: SessionId, bytes: Vec<u8> },
}

/// A drained, per-pane-collapsed batch of output. Panes appear in first-seen
/// order and each pane's bytes are concatenated in arrival order, so applying
/// the batch is exactly one `write_output` and one repaint per dirty pane.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoalescedBatch {
    entries: Vec<(SessionId, Vec<u8>)>,
}

impl CoalescedBatch {
    /// Returns true when the batch touched no panes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates dirty panes in first-seen order with their concatenated bytes.
    pub fn iter(&self) -> impl Iterator<Item = (SessionId, &[u8])> {
        self.entries.iter().map(|(id, bytes)| (*id, bytes.as_slice()))
    }
}

impl IntoIterator for CoalescedBatch {
    type Item = (SessionId, Vec<u8>);
    type IntoIter = std::vec::IntoIter<(SessionId, Vec<u8>)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

/// Collapses a drained run of events into per-pane concatenated output.
///
/// Panes keep first-seen order and each pane's bytes are appended in arrival
/// order, so no output is reordered within a pane and redundant per-pane
/// wakeups collapse to a single entry.
#[must_use]
pub fn coalesce(events: impl IntoIterator<Item = InboundEvent>) -> CoalescedBatch {
    let mut index: HashMap<SessionId, usize> = HashMap::new();
    let mut entries: Vec<(SessionId, Vec<u8>)> = Vec::new();
    for event in events {
        match event {
            InboundEvent::PaneOutput { session_id, bytes } => {
                append_pane_output(&mut index, &mut entries, session_id, bytes);
            }
        }
    }
    CoalescedBatch { entries }
}

/// Appends `bytes` to `session_id`'s buffer, extending an existing entry or
/// starting a new one in first-seen order.
fn append_pane_output(
    index: &mut HashMap<SessionId, usize>,
    entries: &mut Vec<(SessionId, Vec<u8>)>,
    session_id: SessionId,
    bytes: Vec<u8>,
) {
    if let Some(&slot) = index.get(&session_id) {
        if let Some((_, buffered)) = entries.get_mut(slot) {
            buffered.extend_from_slice(&bytes);
        }
        return;
    }
    index.insert(session_id, entries.len());
    entries.push((session_id, bytes));
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

    /// Asks the server to terminate `session_id`, backing the `close_tab`
    /// shortcut. The tab leaves the strip once `SessionExited` arrives.
    ///
    /// # Errors
    /// Returns [`SinkClosed`] when the writer task has dropped its receiver.
    pub fn close_session(&self, session_id: SessionId) -> Result<(), SinkClosed> {
        self.enqueue(ClientMessage::CloseSession { session_id })
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
            guard.push(batch.into_iter().collect());
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
        let collected: Vec<(SessionId, Vec<u8>)> =
            batch.iter().map(|(id, bytes)| (id, bytes.to_vec())).collect();
        assert_eq!(collected, vec![(first, b"123".to_vec()), (second, b"xy".to_vec())]);
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

    // @lat: [[test#GPUI IPC Bridge#Sink reports closed writer]]
    #[test]
    fn key_input_errors_when_writer_dropped() {
        let (out_tx, out_rx) = unbounded_channel::<ClientMessage>();
        drop(out_rx);
        let sink = IpcSink::new(out_tx);
        assert_eq!(sink.key_input(SessionId::new(), b"a".to_vec(), false), Err(SinkClosed));
    }
}
