//! Feature 013 remote (tailnet) chrome state shared between the IPC reader and
//! the GPUI window.
//!
//! The tailnet surface reaches the running client from four directions and they
//! all land here, behind one mutex, exactly as [`crate::lan`] holds the
//! feature-014 LAN surface:
//!
//! * **Discovery / environment.** The startup remote probe's
//!   [`RemoteEnv`](scribe_common::protocol::ServerMessage::RemoteEnv) and
//!   [`RemotePeerList`](scribe_common::protocol::ServerMessage::RemotePeerList)
//!   answers describe this machine's signed-in tailnet account and which
//!   same-account peers are online.
//! * **Dialing side.** When the client itself dialed a peer over the tailnet
//!   ([`crate::remote_handshake`]), the preamble's typed
//!   [`RemoteConnectOutcome`] settles [`RemoteDialStatus`].
//! * **Displacement.** A
//!   [`WindowTakenOver`](scribe_common::protocol::ServerMessage::WindowTakenOver)
//!   parks a [`LostControlState`] here; the window renders the frozen banner
//!   from it and suppresses every keystroke but the one-action reclaim. A
//!   [`RemoteDisconnect`](scribe_common::protocol::ServerMessage::RemoteDisconnect)
//!   records the typed reason the peer severed the link.
//! * **Automation.** A
//!   [`RunAction`](scribe_common::protocol::ServerMessage::RunAction) is queued
//!   here for the foreground to execute, because the action it names (open a
//!   tab, split a pane, focus a session) may only be run on the thread that owns
//!   the window.
//!
//! Everything here is display-independent: the state is a set of plain values
//! plus one derived [`RemoteChrome::status_line`], so the whole module is
//! testable without a window.

use std::collections::VecDeque;

use scribe_common::protocol::{AutomationAction, RemotePeerInfo, RemoteRefusal};

use crate::lost_control::LostControlState;

// Re-exported so the shell reaches the tailnet dial's typed outcome through this
// module rather than through [`crate::remote`], whose remaining surface is the
// (still unported) connect picker.
pub use crate::remote::RemoteConnectOutcome;

/// How many queued [`RunAction`](scribe_common::protocol::ServerMessage::RunAction)
/// requests are held for the foreground before the oldest is dropped.
///
/// Automation is a human-paced surface (`scribe action …` from a shell), and the
/// foreground drains the queue every lifecycle tick, so a backlog this deep only
/// happens when the window is wedged — at which point replaying a minute of
/// stale actions would be worse than losing them.
const MAX_QUEUED_ACTIONS: usize = 16;

/// Where a tailnet dial this client started has got to.
///
/// Only ever leaves [`Idle`](Self::Idle) on a client launched against a tailnet
/// peer; a normal local-socket client stays idle for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteDialStatus {
    /// No tailnet dial was attempted (the local Unix-socket client).
    #[default]
    Idle,
    /// The preamble produced a terminal outcome.
    Settled(RemoteConnectOutcome),
}

/// This machine's own tailnet environment, from the last
/// [`RemoteEnv`](scribe_common::protocol::ServerMessage::RemoteEnv).
///
/// Both fields fail closed: any `LocalAPI` error on the server answers
/// `{ account: None, tailscale_detected: false }`, which is exactly the shape
/// that drives the passive "Tailscale not detected" copy (FR-015).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteEnvSummary {
    /// Signed-in tailnet login name; `None` when unknown.
    pub account: Option<String>,
    /// Whether the server reached `tailscaled` at all.
    pub tailscale_detected: bool,
}

/// Tailnet state the IPC reader folds server answers into and the window
/// renders.
#[derive(Debug, Default)]
pub struct RemoteChrome {
    /// Same-account peers from the last `RemotePeerList`, in server order.
    peers: Vec<RemotePeerInfo>,
    /// This machine's own tailnet environment, once probed.
    env: Option<RemoteEnvSummary>,
    /// Where this client's own tailnet dial has got to.
    dial: RemoteDialStatus,
    /// The displaced-client state a `WindowTakenOver` raised. `Some` freezes the
    /// window: the render pass draws the banner and the key path drops every
    /// keystroke but the reclaim.
    displaced: Option<LostControlState>,
    /// The typed reason the peer severed this connection, from the last
    /// `RemoteDisconnect`.
    severed: Option<RemoteRefusal>,
    /// Automation actions taken off the wire and not yet run by the foreground.
    actions: VecDeque<AutomationAction>,
}

impl RemoteChrome {
    /// Empty state: no peers, no environment, no dial, not displaced.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the peer list from a `RemotePeerList` reply.
    pub fn set_peers(&mut self, peers: Vec<RemotePeerInfo>) {
        self.peers = peers;
    }

    /// The peers from the last `RemotePeerList`.
    #[must_use]
    pub fn peers(&self) -> &[RemotePeerInfo] {
        &self.peers
    }

    /// How many same-account peers are online right now. Offline-but-known peers
    /// are excluded, so this is what a connect affordance would offer.
    #[must_use]
    pub fn online_peer_count(&self) -> usize {
        self.peers.iter().filter(|peer| peer.online).count()
    }

    /// Record this machine's own tailnet environment from a `RemoteEnv` reply.
    pub fn set_env(&mut self, env: RemoteEnvSummary) {
        self.env = Some(env);
    }

    /// This machine's own tailnet environment, once probed.
    #[must_use]
    pub fn env(&self) -> Option<&RemoteEnvSummary> {
        self.env.as_ref()
    }

    /// Settle this client's own tailnet dial on a terminal outcome.
    pub fn settle_dial(&mut self, outcome: RemoteConnectOutcome) {
        self.dial = RemoteDialStatus::Settled(outcome);
    }

    /// Where this client's own tailnet dial has got to.
    #[must_use]
    pub fn dial(&self) -> RemoteDialStatus {
        self.dial
    }

    /// The status bar's controlling-side transport label (feature 014 T025):
    /// present only on a client that itself reached its window over the tailnet.
    #[must_use]
    pub fn transport_label(&self) -> Option<&'static str> {
        matches!(self.dial, RemoteDialStatus::Settled(RemoteConnectOutcome::Accepted))
            .then(|| crate::remote::PeerTransport::Tailnet.label())
    }

    /// Freeze the window under a displaced banner naming the new controller.
    pub fn displace(&mut self, state: LostControlState) {
        self.displaced = Some(state);
    }

    /// The displaced state, when this window has lost control.
    #[must_use]
    pub fn displaced(&self) -> Option<&LostControlState> {
        self.displaced.as_ref()
    }

    /// Clear the displaced state for a user-initiated reclaim.
    ///
    /// Returns whether the window was actually displaced, so the caller only
    /// puts a reclaim on the wire for a banner that was really up.
    pub fn reclaim(&mut self) -> bool {
        self.displaced.take().is_some()
    }

    /// Record the typed reason the peer severed this connection.
    pub fn sever(&mut self, reason: RemoteRefusal) {
        self.severed = Some(reason);
    }

    /// The typed reason the peer severed this connection, if it did.
    #[must_use]
    pub fn severed(&self) -> Option<RemoteRefusal> {
        self.severed
    }

    /// Queue one automation action for the foreground to execute.
    ///
    /// The queue is bounded: past [`MAX_QUEUED_ACTIONS`] the OLDEST request is
    /// dropped, because the newest one is the one the user just typed.
    pub fn queue_action(&mut self, action: AutomationAction) {
        if self.actions.len() >= MAX_QUEUED_ACTIONS {
            let dropped = self.actions.pop_front();
            tracing::warn!(?dropped, "automation queue full; dropped the oldest RunAction");
        }
        self.actions.push_back(action);
    }

    /// Take the next queued automation action, if any.
    pub fn take_action(&mut self) -> Option<AutomationAction> {
        self.actions.pop_front()
    }

    /// One line of user-facing tailnet status, or `None` while there is nothing
    /// to say (an idle dial on a machine with no tailnet environment probed).
    ///
    /// Displacement wins over everything: a window someone else is driving has
    /// nothing more urgent to report. A severed link comes next, then this
    /// client's own dial, and only then the passive environment summary.
    #[must_use]
    pub fn status_line(&self) -> Option<String> {
        if let Some(state) = &self.displaced {
            return Some(state.headline());
        }
        if let Some(reason) = self.severed {
            return Some(format!("Remote connection closed: {}", refusal_text(reason)));
        }
        if let RemoteDialStatus::Settled(outcome) = self.dial {
            return Some(dial_outcome_line(outcome));
        }
        let env = self.env.as_ref()?;
        if !env.tailscale_detected {
            return Some(String::from("Tailscale not detected"));
        }
        let account = env.account.as_deref().unwrap_or("unknown account");
        Some(format!("Tailnet {account}: {} peer(s) online", self.online_peer_count()))
    }
}

/// User-facing copy for a settled tailnet dial, one distinct line per typed
/// refusal so a user can tell "you are not allowed" from "we disagree about
/// versions" (UX-002).
fn dial_outcome_line(outcome: RemoteConnectOutcome) -> String {
    match outcome {
        RemoteConnectOutcome::Accepted => String::from("Connected over Tailscale"),
        RemoteConnectOutcome::ConnectionFailure => String::from("Tailnet connection failed"),
        RemoteConnectOutcome::Refused(reason) => {
            format!("Tailnet connection refused: {}", refusal_text(reason))
        }
    }
}

/// Short human text for a typed [`RemoteRefusal`], shared by the dial copy and
/// the severed-link copy so the two surfaces never drift.
fn refusal_text(reason: RemoteRefusal) -> &'static str {
    match reason {
        RemoteRefusal::Disabled => "remote access is off on the peer",
        RemoteRefusal::Unauthorized => "this account is not authorized",
        RemoteRefusal::IdentityUnavailable => "the peer could not verify your identity",
        RemoteRefusal::IncompatibleVersion => "the peer runs an incompatible version",
        RemoteRefusal::Busy => "the peer's connection limit was reached",
    }
}

#[cfg(test)]
mod tests;
