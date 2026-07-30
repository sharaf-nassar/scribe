//! Synchronized-output frame queueing, ported from the winit client's
//! `pane.rs` / `main.rs` drain path into the GPUI IPC drain task.
//!
//! CSI `?2026` synchronized-update frames must be committed to the terminal as
//! one burst per redraw so a single logical frame never tears across IPC
//! message boundaries. This module sits in front of
//! [`crate::terminal::DisplayOnlyTerminal`]'s `feed_output`: coalesced server
//! bytes are split into committed raw frames by [`SyncUpdateFrameSplitter`],
//! queued per pane, then replayed one committed burst per redraw by
//! [`present_next_burst`]. A 150 ms expiry timer flushes an update whose
//! terminating `CSI ? 2026 l` never arrives, and a catch-up threshold drains
//! through a backlog so stale frames do not pile up indefinitely.
//!
//! A whole-pane rebuild — a decoded `SessionReplay`, or the RIS-prefixed ANSI of
//! a `ScreenSnapshot` — is not output in this sense: it replaces the pane rather
//! than advancing it. [`present_rebuild`] therefore applies it as a burst
//! boundary of its own, never folded into the commit on either side of it.

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
        self.commit_held_bytes()
    }

    /// Commits whatever the splitter is still holding, so the next bytes handed
    /// to the pane cannot be folded into a commit that started before them.
    ///
    /// Used ahead of a whole-pane rebuild, which is a burst boundary in its own
    /// right: an unterminated `CSI ? 2026 h` opened by earlier output would
    /// otherwise swallow the rebuild into the frame it is buffering. Held bytes
    /// are committed rather than discarded so nothing the server already sent is
    /// lost, and the leading BSU is stripped for the same reason the timeout
    /// strips it — the update is being closed early, so replaying it must not
    /// re-enter synchronized-update mode.
    pub fn seal_frame_boundary(&mut self) {
        self.raw_sync_deadline = None;
        self.commit_held_bytes();
    }

    /// Moves the splitter's held bytes onto the frame queue, reporting whether
    /// there were any.
    fn commit_held_bytes(&mut self) -> bool {
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

/// Replays one paced burst — the drain's per-redraw unit of work — reporting
/// whether `target` now owes a repaint.
///
/// This is the pacing wire-in: every caller that advances a pane with ordinary
/// output goes through here rather than emptying the queue, so a caught-up pane
/// presents one committed burst per redraw and only a pane past
/// [`OUTPUT_FRAME_CATCH_UP_THRESHOLD`] is drained through in a single pass. An
/// empty queue and a burst that changed nothing visible are the same answer to
/// the caller — no repaint owed — which is why the two `None`/`false` cases fold
/// into one bool here instead of at each call site.
#[must_use]
pub fn present_next_burst<T: OutputTarget>(queue: &mut SyncFrameQueue, target: &mut T) -> bool {
    drain_until_frame(queue, target).is_some_and(|outcome| outcome.needs_redraw)
}

/// Applies a whole-pane rebuild as a burst boundary of its own, reporting
/// whether `target` now owes a repaint.
///
/// A rebuild is a full state replacement, not an advance, so it is exempt from
/// pacing on both sides. Everything the pane already had queued is committed
/// first, in arrival order, because those bytes describe the screen the server
/// snapshotted; the rebuild itself then reaches `feed_output` whole, bypassing
/// the splitter entirely, because it is one logical frame the server already
/// assembled and re-splitting it could only tear it.
#[must_use]
pub fn present_rebuild<T: OutputTarget>(
    queue: &mut SyncFrameQueue,
    target: &mut T,
    bytes: &[u8],
) -> bool {
    queue.seal_frame_boundary();
    let flushed = drain_all_committed(queue, target);
    flushed.needs_redraw | target.feed_output(bytes).needs_redraw
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
///
/// Deliberately *not* the pacing path: emptying the queue in one pass is what
/// collapsed a run of committed frames into a single redraw. It survives for the
/// one case that has to ignore pacing — clearing a pane ahead of a rebuild that
/// is about to replace it ([`present_rebuild`]) — where holding frames back
/// would only delay bytes the next `feed_output` overwrites anyway.
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
/// one raw frame per commit, ported from
/// `crates/scribe-pty/src/sync_update_filter.rs` so the GPUI client no longer
/// depends on the PTY crate for frame pacing.
///
/// It keeps the original `CSI ? 2026 h/l` bytes in the emitted frames so a
/// sync-aware terminal parser can still apply its normal buffering semantics
/// after frame pacing has been decided.
#[derive(Debug, Default)]
struct SyncUpdateFrameSplitter {
    /// A marker prefix chunked off the end of an earlier message, withheld
    /// until the bytes that complete or break it arrive. Never longer than a
    /// marker minus one byte, and always a strict prefix of one — the scan
    /// resolves it before touching anything else.
    pending: Vec<u8>,
    /// Bytes accumulated toward the frame currently being assembled.
    current: Vec<u8>,
    inside_sync: bool,
    opened_sync_update: bool,
}

/// Start/end synchronized-update escapes.
const BSU_CSI: [u8; 8] = *b"\x1b[?2026h";
const ESU_CSI: [u8; 8] = *b"\x1b[?2026l";
/// The bytes both markers share; only the trailing `h`/`l` tells them apart.
/// A marker therefore contains exactly one [`ESC`], as its first byte, which is
/// what makes the scan below able to restart at the next `ESC` after a failed
/// match instead of backing up one byte at a time.
const SYNC_HEAD: [u8; 7] = *b"\x1b[?2026";
/// The byte every marker starts with, and the only byte a match can start on.
const ESC: u8 = 0x1b;

impl SyncUpdateFrameSplitter {
    /// Preserves sync markers in `input`, returning one raw frame per completed
    /// synchronized-update commit. Bytes outside a sync block are returned
    /// immediately as a tail frame.
    ///
    /// Firehose output is almost entirely marker-free, so the scan jumps to the
    /// next `ESC` and bulk-copies everything before it rather than inspecting
    /// each byte on its own.
    fn split_frames(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        self.opened_sync_update = false;

        let mut remaining = self.resume_pending(input, &mut frames);
        while !remaining.is_empty() {
            let Some(offset) = remaining.iter().position(|byte| *byte == ESC) else {
                self.current.extend_from_slice(remaining);
                break;
            };
            let (plain, candidate) = remaining.split_at(offset);
            self.current.extend_from_slice(plain);

            // `candidate` starts on `ESC`, so the match is at least one byte
            // long and `remaining` always shrinks — the loop cannot spin.
            let (matched, rest) = candidate.split_at(sync_match_len(candidate));
            if matched.len() == BSU_CSI.len() {
                self.commit_marker(matched == BSU_CSI.as_slice(), &mut frames);
                remaining = rest;
                continue;
            }
            if rest.is_empty() {
                // A marker chunked across IPC messages: hold the prefix back
                // until the bytes that complete or break it arrive.
                self.pending.extend_from_slice(matched);
                break;
            }
            self.current.extend_from_slice(matched);
            remaining = rest;
        }

        if !self.inside_sync && !self.current.is_empty() {
            frames.push(std::mem::take(&mut self.current));
        }

        frames
    }

    /// Resolves a marker prefix carried over from an earlier message against the
    /// head of `input`, returning the bytes the marker-free scan resumes on.
    ///
    /// `pending` only ever holds a strict marker prefix, so at most the seven
    /// bytes needed to complete one are examined here; everything past that is
    /// left to the bulk scan.
    fn resume_pending<'a>(&mut self, input: &'a [u8], frames: &mut Vec<Vec<u8>>) -> &'a [u8] {
        if self.pending.is_empty() {
            return input;
        }
        let carried = self.pending.len();
        let (head, tail) = input.split_at(BSU_CSI.len().saturating_sub(carried).min(input.len()));
        let mut buffer = [0u8; BSU_CSI.len()];
        for (slot, byte) in buffer.iter_mut().zip(self.pending.iter().chain(head)) {
            *slot = *byte;
        }
        let (probe, _) = buffer.split_at(carried + head.len());

        let matched = sync_match_len(probe);
        if matched == BSU_CSI.len() {
            self.pending.clear();
            self.commit_marker(probe == BSU_CSI.as_slice(), frames);
            return tail;
        }
        if matched == probe.len() {
            // Still only a prefix, and an exhausted `input` proving it.
            self.pending.extend_from_slice(head);
            return tail;
        }
        // The carried bytes never became a marker. Nothing inside the matched
        // run can start one either (a marker's only `ESC` is its first byte),
        // so it is emitted whole and the scan restarts on the byte that broke
        // the match.
        let (emitted, _) = probe.split_at(matched);
        self.current.extend_from_slice(emitted);
        self.pending.clear();
        let (_, resume) = input.split_at(matched.saturating_sub(carried));
        resume
    }

    /// Records a whole marker, opening the synchronized update or closing it
    /// and committing the frame it accumulated.
    fn commit_marker(&mut self, begin: bool, frames: &mut Vec<Vec<u8>>) {
        if begin {
            self.current.extend_from_slice(&BSU_CSI);
            self.opened_sync_update = !self.inside_sync;
            self.inside_sync = true;
        } else {
            self.current.extend_from_slice(&ESU_CSI);
            self.inside_sync = false;
            self.push_current_frame(frames);
        }
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
        // Whatever is pending is a marker prefix that never completed, so it
        // belongs to the visible bytes rather than to a marker.
        self.current.append(&mut self.pending);

        self.inside_sync = false;

        if self.current.starts_with(&BSU_CSI) {
            self.current.drain(..BSU_CSI.len());
        }

        (!self.current.is_empty()).then(|| std::mem::take(&mut self.current))
    }

    fn push_current_frame(&mut self, frames: &mut Vec<Vec<u8>>) {
        if let Some(frame) = (!self.current.is_empty()).then(|| std::mem::take(&mut self.current)) {
            frames.push(frame);
        }
    }
}

/// How many leading bytes of `bytes` continue a `CSI ? 2026 h/l` match, capped
/// at the marker length and never past `bytes.len()`.
///
/// A full [`BSU_CSI`] length means a whole marker. Anything shorter is either a
/// partial marker still waiting on later bytes (when it consumed all of
/// `bytes`) or the offset at which the match broke.
fn sync_match_len(bytes: &[u8]) -> usize {
    let matched =
        bytes.iter().zip(SYNC_HEAD.iter()).take_while(|(byte, head)| byte == head).count();
    if matched < SYNC_HEAD.len() {
        return matched;
    }
    match bytes.get(SYNC_HEAD.len()) {
        Some(b'h' | b'l') => BSU_CSI.len(),
        _ => SYNC_HEAD.len(),
    }
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

    // @lat: [[test#GPUI Sync Frame Queue#Restarts a broken marker match at the next escape]]
    #[gpui::test]
    fn broken_marker_matches_restart_at_the_next_escape() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // Everything the scan can mistake for a marker and then have to give
        // back: a near-miss that only differs in its final byte, a bare escape
        // butted against another escape, and a short CSI. Every byte has to
        // reach the terminal in order, and the real marker after them still has
        // to commit as its own frame.
        queue.queue_output_frames(b"\x1b[?2026Xtail\x1b\x1b[A");
        queue.queue_output_frames(&[BSU, b"body", ESU].concat());
        drain_all_committed(&mut queue, &mut target);

        assert_eq!(
            target.frames,
            vec![b"\x1b[?2026Xtail\x1b\x1b[A".to_vec(), b"\x1b[?2026hbody\x1b[?2026l".to_vec(),]
        );
    }

    // @lat: [[test#GPUI Sync Frame Queue#Releases a marker prefix that never completes]]
    #[gpui::test]
    fn a_withheld_prefix_that_never_completes_is_released_as_output() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // The first message ends on what could still become a marker, so those
        // bytes are withheld. The next message proves they were ordinary output
        // and they have to be released ahead of the real marker behind them.
        queue.queue_output_frames(b"before\x1b[?2");
        queue.queue_output_frames(&[b"026", BSU, b"body", ESU].concat());
        drain_all_committed(&mut queue, &mut target);

        assert_eq!(
            target.frames,
            vec![b"before".to_vec(), b"\x1b[?2026\x1b[?2026hbody\x1b[?2026l".to_vec(),]
        );
    }

    // @lat: [[test#GPUI Sync Frame Queue#Passes a run of bare escapes through intact]]
    #[gpui::test]
    fn a_run_of_bare_escapes_passes_through_intact() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // Worst case for the scan: every byte restarts a match. Nothing may be
        // swallowed, and only the trailing escape may be withheld.
        let escapes = vec![ESC; 64];
        queue.queue_output_frames(&escapes);
        queue.queue_output_frames(b"x");
        drain_all_committed(&mut queue, &mut target);

        assert_eq!(target.frames.concat(), [escapes, b"x".to_vec()].concat());
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

    // @lat: [[test#GPUI Sync Frame Queue#Applies a rebuild as its own burst]]
    #[gpui::test]
    fn a_rebuild_is_applied_as_its_own_burst() {
        let mut queue = SyncFrameQueue::default();
        let mut target = Recorder::default();

        // A committed frame pacing has not presented yet, followed by a
        // synchronized update still open when the rebuild arrives: the state in
        // which a rebuild folded into the output stream would be swallowed by
        // someone else's frame instead of replacing the pane.
        queue.queue_output_frames(&[BSU, b"queued", ESU].concat());
        queue.queue_output_frames(&[BSU, b"held"].concat());
        assert!(queue.raw_sync_deadline().is_some(), "the open update armed an expiry");

        assert!(present_rebuild(&mut queue, &mut target, b"rebuilt"));

        // Everything queued ahead of the rebuild lands first and whole, the
        // half-open update is sealed rather than dropped, and the rebuild itself
        // reaches the target as a frame of its own.
        assert_eq!(
            target.frames,
            vec![b"\x1b[?2026hqueued\x1b[?2026l".to_vec(), b"held".to_vec(), b"rebuilt".to_vec()]
        );
        assert!(!queue.has_frames());
        assert!(queue.raw_sync_deadline().is_none(), "the sealed update owes no expiry");
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
