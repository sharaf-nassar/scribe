//! Synchronized-output frame queueing, ported from the winit client's
//! `pane.rs` / `main.rs` drain path into the GPUI IPC drain task.
//!
//! CSI `?2026` synchronized-update frames must be committed to the terminal as
//! one burst per redraw so a single logical frame never tears across IPC
//! message boundaries. This module sits in front of
//! [`crate::terminal::DisplayOnlyTerminal`]'s `feed_output`: coalesced server
//! bytes are split into committed raw frames by [`SyncUpdateFrameSplitter`],
//! queued per pane, then replayed one committed burst per redraw. A 150 ms
//! expiry timer flushes an update whose terminating `CSI ? 2026 l` never
//! arrives, and a catch-up threshold drains through a backlog so stale frames
//! do not pile up indefinitely.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Mirror vte's synchronized-update timeout for raw frames still buffered
/// ahead of the pane-local ANSI processor. Matches the winit client's
/// `RAW_SYNC_TIMEOUT`.
pub const RAW_SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// Number of queued committed frames above which the drain stops presenting
/// one burst per redraw and drains through the backlog to the latest frame.
/// Matches the winit client's `OUTPUT_FRAME_CATCH_UP_THRESHOLD`.
pub const OUTPUT_FRAME_CATCH_UP_THRESHOLD: usize = 4;

/// Result of feeding one committed frame into an [`OutputTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedOutputResult {
    /// `true` when the processed bytes changed visible terminal state and the
    /// target should be repainted.
    pub needs_redraw: bool,
    /// `true` when a synchronized update is still open in the parser and may
    /// require a timeout-based flush if no terminating `CSI ? 2026 l` arrives.
    pub sync_pending: bool,
}

/// The terminal-facing side of the drain: whatever consumes committed frames.
///
/// [`crate::terminal::DisplayOnlyTerminal`] is the production implementation;
/// tests use a lightweight recorder. The parser-side synchronized-update
/// timeout is not on this trait because it is driven directly on the concrete
/// terminal by the expiry flusher, not through the generic drain.
pub trait OutputTarget {
    /// Advances the target with one committed frame and reports whether it
    /// changed visible state and whether a sync update is still open.
    fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult;
}

/// Per-pane synchronized-output frame queue. Owns the streaming splitter that
/// preserves `CSI ? 2026 h/l` boundaries across IPC chunking, the FIFO of
/// committed raw frames awaiting replay, and the raw-frame expiry deadline.
#[derive(Debug, Default)]
pub struct SyncFrameQueue {
    /// Committed raw frames queued behind the current redraw. Light bursts
    /// animate incrementally; larger backlogs are drained through.
    pending_output_frames: VecDeque<Vec<u8>>,
    /// Streaming splitter preserving raw sync markers across IPC message splits.
    splitter: SyncUpdateFrameSplitter,
    /// Expiry for raw synchronized-update bytes buffered inside the splitter,
    /// ahead of the target's parser.
    raw_sync_deadline: Option<Instant>,
}

impl SyncFrameQueue {
    /// Queues raw PTY output frames, preserving synchronized-update commit
    /// boundaries across IPC message splits. Returns `true` when at least one
    /// committed frame was enqueued. Ported from
    /// `crates/scribe-client/src/pane.rs` `Pane::queue_output_frames`.
    pub fn queue_output_frames(&mut self, bytes: &[u8]) -> bool {
        let frames = self.splitter.split_frames(bytes);
        self.raw_sync_deadline = if self.splitter.inside_sync() {
            if self.splitter.opened_sync_update() {
                Some(Instant::now() + RAW_SYNC_TIMEOUT)
            } else {
                self.raw_sync_deadline
            }
        } else {
            None
        };
        if frames.is_empty() {
            return false;
        }
        self.pending_output_frames.extend(frames);
        true
    }

    /// Whether any committed frames are queued for replay.
    #[must_use]
    pub fn has_frames(&self) -> bool {
        !self.pending_output_frames.is_empty()
    }

    /// Deadline of the raw synchronized-update buffered inside the splitter, if
    /// one is open.
    #[must_use]
    pub fn raw_sync_deadline(&self) -> Option<Instant> {
        self.raw_sync_deadline
    }

    /// Flushes a raw synchronized update whose deadline has passed, appending
    /// its BSU-stripped bytes to the frame queue in FIFO order so a timeout
    /// cannot overtake an earlier commit or re-enter sync mode. Returns `true`
    /// when bytes were flushed. Ported from the raw side of
    /// `Pane::flush_sync_timeout`.
    pub fn flush_raw_timeout(&mut self, now: Instant) -> bool {
        if self.raw_sync_deadline.is_none_or(|deadline| deadline > now) {
            return false;
        }
        self.raw_sync_deadline = None;
        if let Some(bytes) = self.splitter.flush_timed_out() {
            self.pending_output_frames.push_back(bytes);
            true
        } else {
            false
        }
    }
}

/// Whether committed frames remain queued after a drain. An enum (rather than a
/// bool) keeps [`DrainOutcome`] under the struct-bool budget and mirrors the
/// winit client's `DrainQueueState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// Committed frames remain queued.
    HasMore,
    /// The queue was fully drained.
    Drained,
}

impl QueueState {
    fn from_has_more(has_more: bool) -> Self {
        if has_more { Self::HasMore } else { Self::Drained }
    }
}

/// Outcome of a single [`drain_until_frame`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    /// `true` when the frames applied in this drain changed visible state.
    pub needs_redraw: bool,
    /// Whether committed frames remain queued after this drain.
    pub queue_state: QueueState,
    /// `true` when a synchronized update is still open in the target's parser.
    pub sync_pending: bool,
}

/// Replays committed frames from `queue` into `target`, stopping after the
/// first redraw-worthy burst while the pane is caught up. Once the backlog
/// crosses [`OUTPUT_FRAME_CATCH_UP_THRESHOLD`], it drains through older bursts
/// so stale frames do not pile up. Returns `None` when the queue is empty.
///
/// Ported from `crates/scribe-client/src/main.rs`
/// `App::drain_pane_output_until_frame` + `App::apply_next_pane_output_frame`.
pub fn drain_until_frame<T: OutputTarget>(
    queue: &mut SyncFrameQueue,
    target: &mut T,
) -> Option<DrainOutcome> {
    let mut sync_pending = false;
    let catch_up_to_latest = queue.pending_output_frames.len() > OUTPUT_FRAME_CATCH_UP_THRESHOLD;

    loop {
        let bytes = queue.pending_output_frames.pop_front()?;
        let feed = target.feed_output(&bytes);
        let has_more = queue.has_frames();
        sync_pending |= feed.sync_pending;
        let keep_draining = catch_up_to_latest && has_more;

        if !keep_draining && (feed.needs_redraw || !has_more) {
            return Some(DrainOutcome {
                needs_redraw: feed.needs_redraw,
                queue_state: QueueState::from_has_more(has_more),
                sync_pending,
            });
        }
    }
}

/// Aggregate result of draining every queued committed frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainSummary {
    /// `true` when any drained burst changed visible state.
    pub needs_redraw: bool,
    /// `true` when any drained burst left a synchronized update open.
    pub sync_pending: bool,
}

/// Drains every queued committed frame into `target`, one committed burst per
/// `feed_output`, so no `write_output` ever receives a torn sync frame. Ported
/// from the drain loop in `App::flush_session_output_now`.
pub fn drain_all_committed<T: OutputTarget>(
    queue: &mut SyncFrameQueue,
    target: &mut T,
) -> DrainSummary {
    let mut summary = DrainSummary::default();
    while queue.has_frames() {
        let Some(outcome) = drain_until_frame(queue, target) else { break };
        summary.needs_redraw |= outcome.needs_redraw;
        summary.sync_pending |= outcome.sync_pending;
    }
    summary
}

/// Streaming splitter that preserves raw synchronized-update markers and emits
/// one raw frame per commit, ported verbatim from
/// `crates/scribe-pty/src/sync_update_filter.rs` so the GPUI client no longer
/// depends on the PTY crate for frame pacing.
///
/// It keeps the original `CSI ? 2026 h/l` bytes in the emitted frames so a
/// sync-aware terminal parser can still apply its normal buffering semantics
/// after frame pacing has been decided.
#[derive(Debug, Default)]
struct SyncUpdateFrameSplitter {
    pending: Vec<u8>,
    current: Vec<u8>,
    inside_sync: bool,
    opened_sync_update: bool,
}

/// Start/end synchronized-update escapes.
const BSU_CSI: [u8; 8] = *b"\x1b[?2026h";
const ESU_CSI: [u8; 8] = *b"\x1b[?2026l";

impl SyncUpdateFrameSplitter {
    /// Preserves sync markers in `input`, returning one raw frame per completed
    /// synchronized-update commit. Bytes outside a sync block are returned
    /// immediately as a tail frame.
    fn split_frames(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        self.opened_sync_update = false;

        for &byte in input {
            self.pending.push(byte);
            if !is_sync_prefix(&self.pending) {
                self.drain_pending_non_sync();
                continue;
            }

            if self.pending == BSU_CSI {
                self.current.extend_from_slice(&BSU_CSI);
                self.pending.clear();
                self.opened_sync_update = !self.inside_sync;
                self.inside_sync = true;
                continue;
            }

            if self.pending == ESU_CSI {
                self.current.extend_from_slice(&ESU_CSI);
                self.pending.clear();
                self.inside_sync = false;
                self.push_current_frame(&mut frames);
            }
        }

        if !self.inside_sync && !self.current.is_empty() {
            frames.push(std::mem::take(&mut self.current));
        }

        frames
    }

    /// Whether a synchronized update is still open.
    fn inside_sync(&self) -> bool {
        self.inside_sync
    }

    /// Whether the most recent [`Self::split_frames`] call opened a new
    /// synchronized-update block.
    fn opened_sync_update(&self) -> bool {
        self.opened_sync_update
    }

    /// Flushes a timed-out synchronized update as visible bytes, stripping the
    /// leading BSU marker so callers can replay buffered content without
    /// re-entering synchronized-update mode after the timeout expired.
    fn flush_timed_out(&mut self) -> Option<Vec<u8>> {
        self.drain_pending_non_sync();
        if !self.pending.is_empty() {
            self.current.append(&mut self.pending);
        }

        self.inside_sync = false;

        if self.current.starts_with(&BSU_CSI) {
            self.current.drain(..BSU_CSI.len());
        }

        (!self.current.is_empty()).then(|| std::mem::take(&mut self.current))
    }

    fn drain_pending_non_sync(&mut self) {
        while !self.pending.is_empty() && !is_sync_prefix(&self.pending) {
            self.current.push(self.pending.remove(0));
        }
    }

    fn push_current_frame(&mut self, frames: &mut Vec<Vec<u8>>) {
        if let Some(frame) = (!self.current.is_empty()).then(|| std::mem::take(&mut self.current)) {
            frames.push(frame);
        }
    }
}

fn is_sync_prefix(bytes: &[u8]) -> bool {
    BSU_CSI.starts_with(bytes) || ESU_CSI.starts_with(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording target: feeds and records exactly the byte frames handed to
    /// `feed_output`, so tests can assert commit-boundary preservation without
    /// a real VTE parser.
    #[derive(Default)]
    struct Recorder {
        frames: Vec<Vec<u8>>,
    }

    impl OutputTarget for Recorder {
        fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
            self.frames.push(bytes.to_vec());
            // A committed raw frame always carries visible content in these
            // tests, so every applied frame is redraw-worthy.
            FeedOutputResult { needs_redraw: !bytes.is_empty(), sync_pending: false }
        }
    }

    const BSU: &[u8] = b"\x1b[?2026h";
    const ESU: &[u8] = b"\x1b[?2026l";

    // @lat: [[test#GPUI Sync Frame Queue#Splits committed burst across IPC boundaries]]
    #[gpui::test]
    fn commits_sync_burst_split_across_ipc_messages_as_one_frame() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // The synchronized-update frame is fragmented exactly the way the IPC
        // reader might chunk it: the BSU escape is split mid-sequence, the body
        // straddles two more messages, and the ESU arrives last.
        assert!(!queue.queue_output_frames(b"\x1b[?20"));
        assert!(!queue.queue_output_frames(b"26hpart-one"));
        assert!(!queue.queue_output_frames(b"part-two"));
        assert!(queue.queue_output_frames(b"\x1b[?2026l"));

        let summary = drain_all_committed(&mut queue, &mut target);
        assert!(summary.needs_redraw);
        // Exactly one committed burst reaches the target, reassembled whole
        // with its original sync markers despite the four-way IPC split.
        assert_eq!(target.frames, vec![b"\x1b[?2026hpart-onepart-two\x1b[?2026l".to_vec()]);
        assert!(!queue.has_frames());
    }

    // @lat: [[test#GPUI Sync Frame Queue#Preserves per-commit boundaries]]
    #[gpui::test]
    fn each_commit_is_a_distinct_frame() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        queue.queue_output_frames(b"tail");
        queue.queue_output_frames(&[BSU, b"a", ESU].concat());
        queue.queue_output_frames(&[BSU, b"b", ESU].concat());
        drain_all_committed(&mut queue, &mut target);

        assert_eq!(
            target.frames,
            vec![
                b"tail".to_vec(),
                b"\x1b[?2026ha\x1b[?2026l".to_vec(),
                b"\x1b[?2026hb\x1b[?2026l".to_vec(),
            ]
        );
    }

    // @lat: [[test#GPUI Sync Frame Queue#Presents one burst per redraw when caught up]]
    #[gpui::test]
    fn caught_up_pane_presents_one_burst_per_redraw() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // Two committed frames, below the catch-up threshold.
        queue.queue_output_frames(&[BSU, b"one", ESU].concat());
        queue.queue_output_frames(&[BSU, b"two", ESU].concat());

        let first = drain_until_frame(&mut queue, &mut target).expect("a frame");
        assert!(first.needs_redraw);
        assert_eq!(first.queue_state, QueueState::HasMore);
        assert_eq!(target.frames.len(), 1, "only the first burst is presented this redraw");

        let second = drain_until_frame(&mut queue, &mut target).expect("second frame");
        assert_eq!(second.queue_state, QueueState::Drained);
        assert_eq!(target.frames.len(), 2);
        assert!(drain_until_frame(&mut queue, &mut target).is_none());
    }

    // @lat: [[test#GPUI Sync Frame Queue#Drains through backlog past threshold]]
    #[gpui::test]
    fn backlog_past_threshold_drains_to_latest_frame() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // Six committed frames — above OUTPUT_FRAME_CATCH_UP_THRESHOLD (4).
        for i in 0..6u8 {
            queue.queue_output_frames(&[BSU, &[b'0' + i], ESU].concat());
        }
        assert!(queue.pending_output_frames.len() > OUTPUT_FRAME_CATCH_UP_THRESHOLD);

        let outcome = drain_until_frame(&mut queue, &mut target).expect("drained");
        // A single drain catches up through the entire backlog in one call.
        assert_eq!(outcome.queue_state, QueueState::Drained);
        assert_eq!(target.frames.len(), 6);
        assert!(!queue.has_frames());
    }

    // @lat: [[test#GPUI Sync Frame Queue#Flushes raw sync update on expiry]]
    #[gpui::test]
    fn raw_sync_update_flushes_after_expiry() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // Open a sync update that never terminates: no committed frame yet, but
        // a raw expiry deadline is armed.
        assert!(!queue.queue_output_frames(&[BSU, b"pending"].concat()));
        assert!(!queue.has_frames());
        let deadline = queue.raw_sync_deadline().expect("expiry armed");

        // Before the deadline nothing flushes.
        let before_deadline = deadline.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(!queue.flush_raw_timeout(before_deadline));
        assert!(!queue.has_frames());

        // At the deadline the buffered bytes flush as a BSU-stripped frame.
        assert!(queue.flush_raw_timeout(deadline));
        let summary = drain_all_committed(&mut queue, &mut target);
        assert!(summary.needs_redraw);
        assert_eq!(target.frames, vec![b"pending".to_vec()]);
        assert!(queue.raw_sync_deadline().is_none());
    }
}
