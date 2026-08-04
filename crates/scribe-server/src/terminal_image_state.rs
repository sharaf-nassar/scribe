//! Server-owned ordering seam for one terminal session's image state.
//!
//! The seam consumes the production PTY graphics framer and returns typed,
//! caller-owned boundaries. Live IPC fanout and PTY reply write-back remain
//! downstream integration work.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::OnceLock;

use scribe_common::terminal_images::{
    ImageLimits, TerminalImageDefinition, TerminalImageGeneration, TerminalImageId,
    TerminalImagePlacement, TerminalOutputSequence, TerminalPlacementId, TerminalScreenKind,
};
use scribe_pty::graphics_framing::{
    GraphicsEvent, GraphicsFailure, GraphicsFramer, KittyCommand, PendingGraphicsTransfer,
    RawByteRange, RawBytes, SixelCommand, SixelMode,
};

/// Immutable process policy shared by every session image seam.
#[derive(Debug)]
pub struct TerminalImageProcessPolicy {
    limits: ImageLimits,
    output_sequence_ceiling: u64,
}

impl TerminalImageProcessPolicy {
    /// Construct the frozen terminal-images-v1 process policy.
    #[must_use]
    pub fn v1() -> Arc<Self> {
        static POLICY: OnceLock<Arc<TerminalImageProcessPolicy>> = OnceLock::new();
        Arc::clone(POLICY.get_or_init(|| {
            Arc::new(Self { limits: ImageLimits::V1, output_sequence_ceiling: u64::MAX })
        }))
    }

    /// Construct immutable v1 policy with a smaller sequence ceiling for
    /// deterministic exhaustion validation through the production seam.
    #[must_use]
    pub fn with_sequence_ceiling_for_validation(output_sequence_ceiling: u64) -> Arc<Self> {
        Arc::new(Self { limits: ImageLimits::V1, output_sequence_ceiling })
    }

    /// Return a copy of the immutable process limits.
    #[must_use]
    pub fn limits(&self) -> ImageLimits {
        self.limits
    }
}

/// Image-side meaning of one ordered graphics boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalImageBoundary {
    Kitty(KittyCommand),
    Sixel(SixelCommand),
    SixelMode { mode: SixelMode, enabled: bool },
    Failure(GraphicsFailure),
}

/// One output from production framing, in original PTY byte order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTerminalOutput {
    /// Bytes to feed to the ordinary terminal exactly once.
    Raw(RawBytes),
    /// An image boundary assigned the session's monotonic output sequence.
    Image { sequence: TerminalOutputSequence, range: RawByteRange, boundary: TerminalImageBoundary },
}

/// Caller-owned result of one PTY read; no output is fanned out by the seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTerminalCommit {
    pub generation: TerminalImageGeneration,
    pub through_sequence: TerminalOutputSequence,
    pub outputs: Vec<SessionTerminalOutput>,
}

/// Failure at the session ordering seam before any image state is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminalError {
    SequenceExhausted,
}

impl fmt::Display for SessionTerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => {
                formatter.write_str("terminal image output sequence exhausted")
            }
        }
    }
}

impl std::error::Error for SessionTerminalError {}

/// Payload-free state facts used by production inspection and functional gates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTerminalState {
    pub generation: TerminalImageGeneration,
    pub sequence: TerminalOutputSequence,
    pub active_screen: TerminalScreenKind,
    pub definition_count: usize,
    pub placement_count: usize,
    pub pending_transfer: Option<PendingGraphicsTransfer>,
}

/// Exact payload-free work counters for the framing commit strategy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionTerminalFramingWork {
    /// Reads parsed directly after the input-length boundary preflight.
    pub direct_reads: u64,
    /// Reads parsed against a cloned framer because sequence space was tight.
    pub speculative_clone_reads: u64,
}

/// Authoritative image-state ownership seam for one server terminal session.
// @lat: [[terminal-images#Terminal Images#Server-Owned Session State Seam]]
pub struct SessionTerminal {
    policy: Arc<TerminalImageProcessPolicy>,
    framer: GraphicsFramer,
    generation: TerminalImageGeneration,
    sequence: TerminalOutputSequence,
    active_screen: TerminalScreenKind,
    definitions: BTreeMap<TerminalImageId, TerminalImageDefinition>,
    placements: BTreeMap<
        (TerminalScreenKind, TerminalImageId, TerminalPlacementId),
        TerminalImagePlacement,
    >,
    pending_transfer: Option<PendingGraphicsTransfer>,
    framing_work: SessionTerminalFramingWork,
}

impl SessionTerminal {
    /// Construct a session from its process owner's shared immutable policy.
    #[must_use]
    pub fn new(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        let max_control_string_bytes =
            usize::try_from(policy.limits.max_control_string_bytes).unwrap_or(usize::MAX);
        Self {
            policy,
            framer: GraphicsFramer::with_max_control_string_bytes(max_control_string_bytes),
            generation: TerminalImageGeneration(1),
            sequence: TerminalOutputSequence(0),
            active_screen: TerminalScreenKind::Primary,
            definitions: BTreeMap::new(),
            placements: BTreeMap::new(),
            pending_transfer: None,
            framing_work: SessionTerminalFramingWork::default(),
        }
    }

    /// Consume one PTY read and return raw/image boundaries without fanout.
    pub fn process_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<SessionTerminalCommit, SessionTerminalError> {
        // Every input byte can complete at most one non-Raw boundary: active
        // completion/failure consumes that byte, candidate fallback can only
        // reprocess it as ground input, and SixelMode's companion Raw event is
        // not sequenced. When this conservative bound fits, mutate the framer
        // directly and avoid cloning a retained transfer of up to 16 MiB.
        let boundary_upper_bound =
            u64::try_from(bytes.len()).map_err(|_| SessionTerminalError::SequenceExhausted)?;
        let direct_through = self
            .sequence
            .0
            .checked_add(boundary_upper_bound)
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling);

        if direct_through.is_some() {
            let events = self.framer.push(bytes);
            self.record_direct_read();
            return Ok(self.commit_events(events, None));
        }

        // Only reads close enough to sequence exhaustion to fail the safe
        // upper bound need rollback parsing. The original framer and all
        // canonical state remain untouched when actual emitted events exceed
        // the remaining sequence capacity.
        let mut candidate_framer = self.framer.clone();
        self.record_speculative_clone();
        let events = candidate_framer.push(bytes);
        let through_sequence = self.preflight_sequence(&events)?;
        self.framer = candidate_framer;
        Ok(self.commit_events(events, Some(through_sequence)))
    }

    fn commit_events(
        &mut self,
        events: Vec<GraphicsEvent>,
        admitted_sequence: Option<TerminalOutputSequence>,
    ) -> SessionTerminalCommit {
        let mut output_sequence = self.sequence;
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {
            self.append_event(event, &mut output_sequence, &mut outputs);
        }
        if let Some(admitted_sequence) = admitted_sequence {
            assert_eq!(
                output_sequence, admitted_sequence,
                "sequence preflight must equal committed image boundary count"
            );
        }
        self.sequence = output_sequence;
        self.pending_transfer = self.framer.pending_transfer();
        SessionTerminalCommit {
            generation: self.generation,
            through_sequence: self.sequence,
            outputs,
        }
    }

    fn record_direct_read(&mut self) {
        let direct_reads = self.framing_work.direct_reads.checked_add(1);
        assert!(direct_reads.is_some(), "framing work counter exhausted");
        self.framing_work.direct_reads = direct_reads.unwrap_or(self.framing_work.direct_reads);
    }

    fn record_speculative_clone(&mut self) {
        let speculative_clone_reads = self.framing_work.speculative_clone_reads.checked_add(1);
        assert!(speculative_clone_reads.is_some(), "framing work counter exhausted");
        self.framing_work.speculative_clone_reads =
            speculative_clone_reads.unwrap_or(self.framing_work.speculative_clone_reads);
    }

    /// Record the screen selected by the production terminal observer.
    pub fn observe_active_screen(&mut self, screen: TerminalScreenKind) {
        self.active_screen = screen;
    }

    /// Return payload-free ownership facts.
    #[must_use]
    pub fn state(&self) -> SessionTerminalState {
        SessionTerminalState {
            generation: self.generation,
            sequence: self.sequence,
            active_screen: self.active_screen,
            definition_count: self.definitions.len(),
            placement_count: self.placements.len(),
            pending_transfer: self.pending_transfer,
        }
    }

    /// Return exact work counters without exposing retained image payloads.
    #[must_use]
    pub fn framing_work(&self) -> SessionTerminalFramingWork {
        self.framing_work
    }

    /// Confirm that two sessions use the exact same process policy object.
    #[must_use]
    pub fn shares_process_policy_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.policy, &other.policy)
    }

    fn preflight_sequence(
        &self,
        events: &[GraphicsEvent],
    ) -> Result<TerminalOutputSequence, SessionTerminalError> {
        let image_boundaries =
            events.iter().filter(|event| !matches!(event, GraphicsEvent::Raw(_))).count();
        let image_boundaries =
            u64::try_from(image_boundaries).map_err(|_| SessionTerminalError::SequenceExhausted)?;
        self.sequence
            .0
            .checked_add(image_boundaries)
            .filter(|sequence| *sequence <= self.policy.output_sequence_ceiling)
            .map(TerminalOutputSequence)
            .ok_or(SessionTerminalError::SequenceExhausted)
    }

    fn append_event(
        &self,
        event: GraphicsEvent,
        sequence: &mut TerminalOutputSequence,
        outputs: &mut Vec<SessionTerminalOutput>,
    ) {
        match event {
            GraphicsEvent::Raw(raw) => outputs.push(SessionTerminalOutput::Raw(raw)),
            GraphicsEvent::Kitty { range, command } => {
                self.append_image(range, TerminalImageBoundary::Kitty(command), sequence, outputs);
            }
            GraphicsEvent::Sixel { range, command } => {
                self.append_image(range, TerminalImageBoundary::Sixel(command), sequence, outputs);
            }
            GraphicsEvent::SixelMode(change) => {
                let range = change.raw.range;
                outputs.push(SessionTerminalOutput::Raw(change.raw));
                self.append_image(
                    range,
                    TerminalImageBoundary::SixelMode { mode: change.mode, enabled: change.enabled },
                    sequence,
                    outputs,
                );
            }
            GraphicsEvent::Failure(failure) => {
                let range = failure.range;
                self.append_image(
                    range,
                    TerminalImageBoundary::Failure(failure),
                    sequence,
                    outputs,
                );
            }
        }
    }

    fn append_image(
        &self,
        range: RawByteRange,
        boundary: TerminalImageBoundary,
        sequence: &mut TerminalOutputSequence,
        outputs: &mut Vec<SessionTerminalOutput>,
    ) {
        let next =
            sequence.0.checked_add(1).filter(|next| *next <= self.policy.output_sequence_ceiling);
        assert!(next.is_some(), "sequence preflight admitted every image boundary");
        let next = next.unwrap_or(sequence.0);
        *sequence = TerminalOutputSequence(next);
        outputs.push(SessionTerminalOutput::Image { sequence: *sequence, range, boundary });
    }
}

/// Production PTY-reader ownership for exactly one terminal-image seam.
// @lat: [[terminal-images#Terminal Images#Server-Owned Session State Seam]]
pub struct PtyTerminalImageState {
    terminal: SessionTerminal,
}

impl PtyTerminalImageState {
    /// Construct the reader-owned seam from process policy.
    #[must_use]
    pub fn new(policy: Arc<TerminalImageProcessPolicy>) -> Self {
        Self { terminal: SessionTerminal::new(policy) }
    }

    /// Return payload-free state facts for diagnostics and evidence.
    #[must_use]
    pub fn state(&self) -> SessionTerminalState {
        self.terminal.state()
    }

    /// Return exact framing work counters for allocation evidence.
    #[must_use]
    pub fn framing_work(&self) -> SessionTerminalFramingWork {
        self.terminal.framing_work()
    }
}

/// Route one effective PTY chunk through the production image, client, and
/// terminal sinks exactly once and in that order.
///
/// Image rejection does not suppress ordinary terminal delivery. Image fanout
/// and PTY reply write-back remain deliberately absent from this shared path.
pub async fn process_pty_reader_ingress<Bytes, Deliver, Feed, FeedFuture>(
    terminal_images: &mut PtyTerminalImageState,
    bytes: Bytes,
    deliver: Deliver,
    feed: Feed,
) -> Result<SessionTerminalCommit, SessionTerminalError>
where
    Bytes: AsRef<[u8]>,
    Deliver: FnOnce(&[u8]),
    Feed: FnOnce(Bytes) -> FeedFuture,
    FeedFuture: Future<Output = ()>,
{
    let image_result = terminal_images.terminal.process_bytes(bytes.as_ref());
    deliver(bytes.as_ref());
    feed(bytes).await;
    image_result
}
