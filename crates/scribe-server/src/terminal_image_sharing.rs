//! Session image capability lifecycle, PTY reply write-back, and capable-sink
//! fanout policy.
//!
//! The seam in [`crate::terminal_image_state`] owns canonical image state and
//! deliberately fans nothing out. This module owns the three things that turn
//! that state into observable behavior for applications and viewers:
//!
//! 1. the server-owned capability latch, which survives detach and decides who
//!    may attach at all;
//! 2. the ordered replies one committed read owes the originating PTY, written
//!    exactly once through the server's internal write path — never through a
//!    client's `KeyInput`; and
//! 3. which attached sinks are allowed to receive typed image records.
//!
//! Everything here is payload-free: replies carry protocol control data and
//! stable status codes, never pixels, base64, paths, or decoded bytes.

use std::borrow::Cow;
use std::fmt::Write as _;

use scribe_common::terminal_images::{
    TerminalImageCapabilities, TerminalImageCapabilityMismatch, TerminalImageFeatures,
    TerminalOutputSequence,
};
use scribe_pty::graphics_framing::{
    GraphicsFailure, GraphicsFailureCategory, GraphicsProtocol, KittyAction, KittyChunkState,
    KittyCommand,
};

use crate::terminal_image_state::{
    SessionTerminalCommit, SessionTerminalOutput, TerminalImageBoundary,
};

/// Server-owned image capability for one session.
///
/// Capability is session state, not viewer state: a latched session keeps
/// parsing, replying, and retaining bounded state with zero viewers, and an
/// incapable viewer is refused rather than shown a divergent screen.
// @lat: [[terminal-images#Terminal Images#Session Capability Latch]]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionImageSharing {
    /// The exact subset latched by the capable creator or enabling viewer.
    /// `None` is the `text-only-unlatched` phase.
    latched: Option<TerminalImageCapabilities>,
    /// Master kill switch. Off means no new claim and no retained capability.
    master_enabled: bool,
}

/// What one master-switch write actually did, so a caller knows whether it must
/// cancel decode and release retained state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillSwitchTransition {
    /// The switch already held the requested value.
    Unchanged,
    /// Newly disabled. `cleared_latch` is true when a latched session was
    /// returned to `text-only-unlatched` and its state must be released.
    Disabled { cleared_latch: bool },
    /// Newly enabled. Re-enabling never restores a latch; a capable viewer must
    /// claim again.
    Enabled,
}

impl KillSwitchTransition {
    /// Whether this transition obliges the session's reader to cancel decode
    /// work and release retained and committed image state.
    ///
    /// Only a disable that actually cleared a latch owns resources; disabling
    /// an already-text-only session frees nothing, and no enable ever does.
    #[must_use]
    pub const fn releases_state(self) -> bool {
        matches!(self, Self::Disabled { cleared_latch: true })
    }
}

impl SessionImageSharing {
    /// A fresh `text-only-unlatched` session under the current master switch.
    #[must_use]
    pub const fn new(master_enabled: bool) -> Self {
        Self { latched: None, master_enabled }
    }

    /// Whether a capable viewer has latched this session.
    #[must_use]
    pub const fn is_latched(self) -> bool {
        self.latched.is_some()
    }

    /// Whether image parsing, replies, and retention are active right now.
    #[must_use]
    pub const fn images_enabled(self) -> bool {
        self.master_enabled && self.latched.is_some()
    }

    /// The subset this session advertises to a connecting viewer. An unlatched
    /// or disabled session advertises nothing, so discovery cannot mistake a
    /// policy-disabled Scribe for an enabled implementation.
    #[must_use]
    pub fn effective(self) -> TerminalImageCapabilities {
        if self.master_enabled {
            self.latched.unwrap_or_default()
        } else {
            TerminalImageCapabilities::default()
        }
    }

    /// Latch this session to the intersection of the viewer's advertised
    /// renderer capability and Scribe's compile-time v1 subset.
    ///
    /// Latching is idempotent: a second capable viewer joins the existing latch
    /// instead of widening or narrowing it, which is what keeps a detach or a
    /// controller change from silently changing what applications were told.
    pub fn latch(&mut self, viewer: TerminalImageCapabilities) -> TerminalImageCapabilities {
        if !self.master_enabled {
            return TerminalImageCapabilities::default();
        }
        if let Some(latched) = self.latched {
            return latched;
        }
        let claimed = intersect(viewer, TerminalImageCapabilities::V1);
        if claimed.runtime_enabled {
            self.latched = Some(claimed);
        }
        self.effective()
    }

    /// Decide whether `viewer` may attach.
    ///
    /// An unlatched session is ordinary text and admits anyone. A latched
    /// session admits only a viewer that supports everything it latched;
    /// anyone else gets the typed mismatch instead of invisible graphics.
    pub fn admit(
        self,
        viewer: TerminalImageCapabilities,
    ) -> Result<(), TerminalImageCapabilityMismatch> {
        let required = self.effective();
        if !required.runtime_enabled {
            return Ok(());
        }
        TerminalImageCapabilityMismatch::new(required, viewer).map_or(Ok(()), Err)
    }

    /// Write the master switch, reporting what the caller must clean up.
    pub fn set_master_enabled(&mut self, enabled: bool) -> KillSwitchTransition {
        if self.master_enabled == enabled {
            return KillSwitchTransition::Unchanged;
        }
        self.master_enabled = enabled;
        if enabled {
            return KillSwitchTransition::Enabled;
        }
        KillSwitchTransition::Disabled { cleared_latch: self.latched.take().is_some() }
    }
}

/// Process-wide master image switch. Default-on, written by the settings path
/// and read by every capability decision that is not already session state.
static IMAGES_MASTER_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Read the process-wide master image switch.
#[must_use]
pub fn images_master_enabled() -> bool {
    IMAGES_MASTER_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Write the process-wide master image switch, returning the previous value so
/// a caller can tell a real transition from a no-op reload.
pub fn set_images_master_enabled(enabled: bool) -> bool {
    IMAGES_MASTER_ENABLED.swap(enabled, std::sync::atomic::Ordering::Relaxed)
}

/// The image subset one connection is told it has, before any session latch.
///
/// This is the `Welcome` answer: the intersection of the viewer's advertised
/// renderer capability with Scribe's compile-time v1 subset, emptied entirely
/// while the master switch is off so a disabled server advertises nothing.
#[must_use]
pub const fn effective_connection_subset(
    viewer: TerminalImageCapabilities,
    master_enabled: bool,
) -> TerminalImageCapabilities {
    if master_enabled {
        intersect(viewer, TerminalImageCapabilities::V1)
    } else {
        TerminalImageCapabilities {
            runtime_enabled: false,
            features: TerminalImageFeatures::from_bits(0),
        }
    }
}

/// Intersect two capability sets. Runtime enablement is a conjunction and
/// features are a bitwise intersection.
const fn intersect(
    left: TerminalImageCapabilities,
    right: TerminalImageCapabilities,
) -> TerminalImageCapabilities {
    TerminalImageCapabilities {
        runtime_enabled: left.runtime_enabled && right.runtime_enabled,
        features: TerminalImageFeatures::from_bits(left.features.bits() & right.features.bits()),
    }
}

/// One reply the session owes the originating PTY, tagged with the image output
/// sequence that produced it so ordering is auditable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyImageReply {
    /// Image output sequence of the boundary that owes this reply.
    pub sequence: TerminalOutputSequence,
    /// Exact bytes to write to the PTY, in stream order.
    pub bytes: Vec<u8>,
}

/// Build the ordered replies one committed read owes the PTY, exactly once.
///
/// The result is in image-output-sequence order, which is PTY byte order. The
/// caller writes it before draining the terminal's own event queue, so a Kitty
/// result always precedes the DA1 reply an application requests behind its
/// capability probe.
///
/// A disabled session owes nothing: emitting a Kitty result after the capability
/// claim was withdrawn would advertise an implementation that is not running.
// @lat: [[terminal-images#Terminal Images#Exactly-Once PTY Replies]]
#[must_use]
pub fn plan_pty_replies(commit: &SessionTerminalCommit, enabled: bool) -> Vec<PtyImageReply> {
    if !enabled {
        return Vec::new();
    }
    commit
        .outputs
        .as_slice()
        .iter()
        .filter_map(|output| match output {
            SessionTerminalOutput::Image { sequence, boundary, .. } => {
                reply_bytes(boundary).map(|bytes| PtyImageReply { sequence: *sequence, bytes })
            }
            SessionTerminalOutput::Raw(_) => None,
        })
        .collect()
}

/// The reply one boundary owes, or `None` when the protocol defines silence.
fn reply_bytes(boundary: &TerminalImageBoundary) -> Option<Vec<u8>> {
    match boundary {
        TerminalImageBoundary::Kitty { command, .. } => kitty_success_reply(command),
        TerminalImageBoundary::Failure(failure) => kitty_failure_reply(failure),
        // Sixel has no protocol reply; its discovery is DA1 attribute 4.
        TerminalImageBoundary::Sixel { .. } | TerminalImageBoundary::SixelMode { .. } => None,
    }
}

/// `q=0` normal, `q=1` suppresses success, `q=2` suppresses everything.
const QUIET_SUPPRESS_SUCCESS: u8 = 1;

fn kitty_success_reply(command: &KittyCommand) -> Option<Vec<u8>> {
    if command.quiet >= QUIET_SUPPRESS_SUCCESS {
        return None;
    }
    // A continuation chunk is not a completed command; the transfer replies once
    // its final chunk lands.
    if matches!(command.chunk_state, KittyChunkState::More) {
        return None;
    }
    // Kitty answers a command that named an image; an anonymous put or delete
    // has nothing to echo and stays silent.
    let image_id = command.image_id?;
    let mut reply = format!("\x1b_Gi={image_id}");
    if let Some(placement_id) = command.placement_id {
        // Writing into a `String` is infallible.
        write!(reply, ",p={placement_id}").ok();
    }
    // Every supported action reports the same stable success token; the action
    // is only recorded here so an unsupported one can never reach this path.
    debug_assert!(matches!(
        command.action,
        KittyAction::Transmit
            | KittyAction::TransmitDisplay
            | KittyAction::Put
            | KittyAction::Query
            | KittyAction::Delete
    ));
    reply.push_str(";OK\x1b\\");
    Some(reply.into_bytes())
}

fn kitty_failure_reply(failure: &GraphicsFailure) -> Option<Vec<u8>> {
    if !matches!(failure.protocol, GraphicsProtocol::Kitty) {
        return None;
    }
    // ponytail: a failure annotation carries no quiet level, so `q=2` cannot
    // suppress an error reply the way it suppresses a success reply. Upgrade
    // path: carry the parsed quiet level on `GraphicsFailure` when a real
    // application is observed setting `q=2` and objecting to the error.
    let code = failure_code(failure.category);
    Some(format!("\x1b_G;{code}\x1b\\").into_bytes())
}

/// Stable status codes frozen by the terminal-images contract's failure table.
const fn failure_code(category: GraphicsFailureCategory) -> &'static str {
    match category {
        GraphicsFailureCategory::UnsupportedProtocol
        | GraphicsFailureCategory::UnsupportedAction
        | GraphicsFailureCategory::UnsupportedTransport => "ENOSYS",
        GraphicsFailureCategory::MalformedFraming
        | GraphicsFailureCategory::MalformedControl
        | GraphicsFailureCategory::MalformedPayload
        | GraphicsFailureCategory::TruncatedSequence => "EINVAL",
        GraphicsFailureCategory::QuotaExceeded => "ENOSPC",
    }
}

/// Sixel's DA1 discovery attribute.
const SIXEL_ATTRIBUTE: &str = "4";

/// Append Sixel DA1 attribute `4` to a primary device-attributes reply exactly
/// once, preserving every attribute the terminal already reported.
///
/// Any other terminal reply — DSR, DECRPM, the secondary DA — passes through
/// untouched, and a disabled session never gains the attribute, so discovery
/// stays truthful in both directions.
// @lat: [[terminal-images#Terminal Images#Sixel DA1 Advertisement]]
#[must_use]
pub fn augment_device_attributes(reply: &str, sixel_enabled: bool) -> Cow<'_, str> {
    if !sixel_enabled {
        return Cow::Borrowed(reply);
    }
    let Some(body) = reply.strip_prefix("\x1b[?").and_then(|rest| rest.strip_suffix('c')) else {
        return Cow::Borrowed(reply);
    };
    if body.split(';').any(|attribute| attribute == SIXEL_ATTRIBUTE) {
        return Cow::Borrowed(reply);
    }
    Cow::Owned(format!("\x1b[?{body};{SIXEL_ATTRIBUTE}c"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kitty_only() -> TerminalImageCapabilities {
        TerminalImageCapabilities {
            runtime_enabled: true,
            features: TerminalImageFeatures::from_bits(
                TerminalImageFeatures::KITTY_RGB | TerminalImageFeatures::KITTY_CLASSIC_PLACEMENT,
            ),
        }
    }

    #[test]
    fn a_latch_survives_detach_and_admits_only_capable_viewers() {
        let mut sharing = SessionImageSharing::new(true);
        assert!(!sharing.is_latched());
        assert!(sharing.admit(TerminalImageCapabilities::default()).is_ok());

        let effective = sharing.latch(TerminalImageCapabilities::V1);
        assert_eq!(effective, TerminalImageCapabilities::V1);
        assert!(sharing.images_enabled());
        // Detach is not modeled here because it changes no field: the latch is
        // session state, so the same assertions hold with zero viewers.
        assert!(sharing.admit(TerminalImageCapabilities::V1).is_ok());
        let mismatch = sharing.admit(kitty_only()).expect_err("incapable viewer must be refused");
        assert_eq!(mismatch.required, TerminalImageCapabilities::V1);
        assert_eq!(mismatch.offered, kitty_only());
    }

    #[test]
    fn an_incapable_creator_never_latches() {
        let mut sharing = SessionImageSharing::new(true);
        assert_eq!(
            sharing.latch(TerminalImageCapabilities::default()),
            TerminalImageCapabilities::default()
        );
        assert!(!sharing.is_latched());
    }

    #[test]
    fn the_kill_switch_clears_the_latch_and_never_restores_it() {
        let mut sharing = SessionImageSharing::new(true);
        sharing.latch(TerminalImageCapabilities::V1);

        assert_eq!(
            sharing.set_master_enabled(false),
            KillSwitchTransition::Disabled { cleared_latch: true }
        );
        assert!(!sharing.images_enabled());
        assert_eq!(sharing.effective(), TerminalImageCapabilities::default());
        assert!(sharing.admit(TerminalImageCapabilities::default()).is_ok());
        assert_eq!(sharing.set_master_enabled(false), KillSwitchTransition::Unchanged);

        assert_eq!(sharing.set_master_enabled(true), KillSwitchTransition::Enabled);
        assert!(!sharing.is_latched());
        assert!(sharing.latch(TerminalImageCapabilities::V1).runtime_enabled);
    }

    #[test]
    fn a_disabled_session_refuses_to_latch() {
        let mut sharing = SessionImageSharing::new(false);
        assert_eq!(
            sharing.latch(TerminalImageCapabilities::V1),
            TerminalImageCapabilities::default()
        );
        assert!(!sharing.is_latched());
    }

    #[test]
    fn da1_gains_attribute_four_exactly_once() {
        assert_eq!(augment_device_attributes("\x1b[?6c", true), "\x1b[?6;4c");
        assert_eq!(augment_device_attributes("\x1b[?6;4c", true), "\x1b[?6;4c");
        assert_eq!(augment_device_attributes("\x1b[?6c", false), "\x1b[?6c");
        // Not a primary DA reply.
        assert_eq!(augment_device_attributes("\x1b[>0;95;0c", true), "\x1b[>0;95;0c");
        assert_eq!(augment_device_attributes("\x1b[0n", true), "\x1b[0n");
    }
}
