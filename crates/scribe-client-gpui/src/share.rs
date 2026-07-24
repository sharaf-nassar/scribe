//! Feature 015 (T015/T020) collaborative window-sharing surfaces, ported into the
//! GPUI rebuild.
//!
//! When a window is shared across machines in
//! [`SharedSingleTypist`](scribe_common::config::SharingMode::SharedSingleTypist)
//! the server broadcasts a full-state
//! [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster) on every
//! membership / control change. The client mirrors the latest roster in a
//! [`ShareState`] and derives whether this machine is the current input holder or
//! a **viewer**. A viewer renders the live terminal exactly as normal (never the
//! frozen dimmed [`crate::lost_control::LostControlState`], reserved for
//! `SingleController` displacement) but suppresses its own keystrokes locally and
//! shows a non-intrusive [`ControlHint`].
//!
//! The current holder (or the owner, when control is unheld) answers an incoming
//! request-and-grant
//! [`ControlRequested`](scribe_common::protocol::ServerMessage::ControlRequested)
//! through a [`ControlRequestPrompt`]. Control passing itself is expressed as a
//! [`ControlIntent`] that maps to the frozen v3
//! [`ControlClaim`](scribe_common::protocol::ClientMessage::ControlClaim) /
//! [`ControlRequest`](scribe_common::protocol::ClientMessage::ControlRequest) /
//! [`ControlGrant`](scribe_common::protocol::ClientMessage::ControlGrant)
//! messages. This port keeps that logic and drops the winit `CellInstance`
//! painting in favour of the GPUI overlay elements.

use std::time::{Duration, Instant};

use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, ParticipantInfo};

/// How long a transient control hint / denied notice stays on screen before the
/// idle-wake loop clears it.
pub const HINT_DURATION: Duration = Duration::from_secs(5);

/// A control-passing intent the app emits on the frozen v3 protocol. Kept as a
/// small `PartialEq` enum so the ported logic is testable without constructing a
/// full [`ClientMessage`]; [`ControlIntent::into_message`] performs the mapping.
/// `ControlRequest` remains a serializable alias the server handles as
/// `ControlClaim`, but the client deliberately emits only the modeled variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlIntent {
    /// A participant takes input control directly (`FreeClaim`, or the owner).
    Claim { window_id: WindowId },
    /// A viewer asks for input control (`RequestAndGrant`).
    Request { window_id: WindowId },
    /// The holder/owner answers a pending request: transfer or deny.
    Grant { window_id: WindowId, participant_id: u64, accept: bool },
}

impl ControlIntent {
    /// Lower the intent to its frozen v3 [`ClientMessage`].
    #[must_use]
    pub fn into_message(self) -> ClientMessage {
        match self {
            Self::Claim { window_id } => ClientMessage::ControlClaim { window_id },
            Self::Request { window_id } => ClientMessage::ControlRequest { window_id },
            Self::Grant { window_id, participant_id, accept } => {
                ClientMessage::ControlGrant { window_id, participant_id, accept }
            }
        }
    }
}

/// The latest [`ShareRoster`](scribe_common::protocol::ServerMessage::ShareRoster)
/// for the client's window, mirrored so the UI can derive roles and render the
/// presence badge. Absent on the client whenever the window is not part of a
/// broadcasting share (`SingleController` / solo).
#[derive(Debug, Clone)]
pub struct ShareState {
    /// The shared window's id.
    pub window_id: WindowId,
    /// The complete current participant roster (server order, by join).
    pub participants: Vec<ParticipantInfo>,
    /// The window's active sharing mode.
    pub mode: SharingMode,
    /// The current input-control holder's participant id, or `None` when unheld.
    pub holder: Option<u64>,
}

impl ShareState {
    /// Number of attached participants (owner + remotes).
    #[must_use]
    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    /// Whether more than one machine is attached — the trigger for the presence
    /// badge (T024) and the viewer affordances (T015/T020).
    #[must_use]
    pub fn is_multi(&self) -> bool {
        self.participants.len() > 1
    }

    /// The current control holder's roster entry, if any.
    #[must_use]
    pub fn holder_entry(&self) -> Option<&ParticipantInfo> {
        let holder = self.holder?;
        self.participants.iter().find(|p| p.participant_id == holder)
    }

    /// Display label for the current control holder, or `None` when unheld.
    #[must_use]
    pub fn holder_label(&self) -> Option<String> {
        self.holder_entry().map(participant_label)
    }

    /// Whether the local machine currently holds input control.
    #[must_use]
    pub fn local_is_holder(&self) -> bool {
        self.participants.iter().any(|p| p.is_local && p.is_holder)
    }

    /// The control-passing intent a local viewer emits to take control, chosen by
    /// the window's sharing mode: a single-typist share claims directly under
    /// free-claim acquisition, mirroring the winit client's viewer affordance.
    #[must_use]
    pub fn take_control_intent(&self) -> ControlIntent {
        ControlIntent::Claim { window_id: self.window_id }
    }
}

/// Human-readable label for a roster entry: `device (login)`, or just the device
/// name when the account is unknown (the owner's `Local` entry is named `this
/// machine` server-side, with an empty login).
#[must_use]
pub fn participant_label(p: &ParticipantInfo) -> String {
    if p.login_name.is_empty() {
        p.device_name.clone()
    } else {
        format!("{} ({})", p.device_name, p.login_name)
    }
}

/// A transient, non-intrusive hint shown to a viewer that just pressed a
/// (suppressed) key: it names the control holder and how to take control. Unlike
/// the lost-control banner it does NOT dim or freeze the window — output keeps
/// streaming live behind it (T015/T020).
#[derive(Debug, Clone)]
pub struct ControlHint {
    text: String,
    expires_at: Instant,
}

impl ControlHint {
    /// Build a hint that expires after [`HINT_DURATION`].
    #[must_use]
    pub fn new(text: String) -> Self {
        Self { text, expires_at: Instant::now() + HINT_DURATION }
    }

    /// The hint text drawn in the strip.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The instant this hint should be cleared (for the idle-wake deadline).
    #[must_use]
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Whether this hint has outlived [`HINT_DURATION`].
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// A pending incoming control request that this client — the current holder, or
/// the owner when control is unheld — must grant or deny (request-and-grant
/// acquisition, T020). Modal while set: Enter grants, Esc denies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequestPrompt {
    window_id: WindowId,
    requester_id: u64,
    requester_label: String,
}

impl ControlRequestPrompt {
    /// Build the prompt from the `from` participant of a `ControlRequested`.
    #[must_use]
    pub fn new(window_id: WindowId, from: &ParticipantInfo) -> Self {
        Self {
            window_id,
            requester_id: from.participant_id,
            requester_label: participant_label(from),
        }
    }

    /// The shared window this request targets.
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    /// The requesting participant's id — the [`ControlIntent::Grant`] target.
    #[must_use]
    pub fn requester_id(&self) -> u64 {
        self.requester_id
    }

    /// Banner headline: `<requester> wants control`.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("{} wants control", self.requester_label)
    }

    /// The grant/deny hint drawn under the headline.
    #[must_use]
    pub fn hint() -> &'static str {
        "Press Enter to grant \u{00B7} Esc to deny"
    }

    /// The control-passing intent for the user's decision: `accept = true`
    /// transfers control to the requester, `false` denies.
    #[must_use]
    pub fn answer(&self, accept: bool) -> ControlIntent {
        ControlIntent::Grant {
            window_id: self.window_id,
            participant_id: self.requester_id,
            accept,
        }
    }
}

#[cfg(test)]
mod tests;
