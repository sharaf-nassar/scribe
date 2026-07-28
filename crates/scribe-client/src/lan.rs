//! Feature 014 LAN chrome state shared between the IPC reader and the GPUI
//! window.
//!
//! The LAN surface reaches the running client from three directions and they
//! all land here, behind one mutex, exactly as [`crate::window_lifecycle`] holds
//! the window's own lifecycle:
//!
//! * **Owning side.** This machine's own server pushes a
//!   [`LanApprovalRequest`](scribe_common::protocol::ServerMessage::LanApprovalRequest)
//!   when an unknown device completes the mutual-TLS handshake. The reader parks
//!   the ported [`LanApprovalDialog`] here; the foreground's tick takes it and
//!   raises the modal, whose answer becomes a
//!   [`LanApprovalDecision`](scribe_common::protocol::ClientMessage::LanApprovalDecision).
//!   Parking rather than rendering from the reader thread is what keeps every
//!   GPUI entity touched only from the thread that owns it.
//! * **Discovery / environment.** The startup LAN probe's
//!   [`LanEnv`](scribe_common::protocol::ServerMessage::LanEnv) and
//!   [`LanPeerList`](scribe_common::protocol::ServerMessage::LanPeerList)
//!   answers describe whether this machine's LAN surface is live (own identity
//!   fingerprint, current-network addability) and which peers are reachable on
//!   it.
//! * **Dialing side.** When the client itself dialed a peer
//!   ([`crate::lan_dial`]), the approval gate's
//!   [`LanApprovalPending`](scribe_common::protocol::ServerMessage::LanApprovalPending)
//!   → [`LanApprovalResult`](scribe_common::protocol::ServerMessage::LanApprovalResult)
//!   pair moves [`LanDialStatus`] from waiting to settled.
//!
//! Everything here is display-independent: the state is a set of plain values
//! plus one derived [`LanChrome::status_line`], so the whole module is testable
//! without a window.

use scribe_common::protocol::{LanPeerInfo, LanRefusal};

use crate::lan_approval::LanApprovalDialog;

// Re-exported so the shell reaches the LAN dial's typed outcome through this
// module rather than through [`crate::remote`], whose remaining surface is the
// (still unported) connect picker.
pub use crate::remote::LanConnectOutcome;

/// Where a LAN dial this client started has got to.
///
/// Only ever leaves [`Idle`](Self::Idle) on a client launched against a LAN
/// peer; a normal local-socket client stays idle for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LanDialStatus {
    /// No LAN dial was attempted (the local Unix-socket client).
    #[default]
    Idle,
    /// The peer answered `LanApprovalPending`: an unknown device is held while
    /// the owning user decides. No window or session data has been revealed.
    AwaitingApproval,
    /// The approval gate produced a terminal outcome.
    Settled(LanConnectOutcome),
}

/// This machine's own LAN environment, from the last
/// [`LanEnv`](scribe_common::protocol::ServerMessage::LanEnv).
///
/// Identity fields stay `Option` because they are absent until the device
/// identity has been generated on first LAN enable, and the whole reply fails
/// closed to `None` identity on any local error.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LanEnvSummary {
    /// Lowercase hex of this device's `device_id = SHA-256(SPKI)`.
    pub device_id_hex: Option<String>,
    /// This device's own fingerprint words, for the out-of-band MITM compare.
    pub fingerprint_words: Option<String>,
    /// Whether the network this machine is currently on can be fingerprinted
    /// and therefore marked trusted.
    pub current_network_addable: bool,
    /// Short note explaining why it cannot, when it cannot.
    pub current_network_reason: Option<String>,
}

/// LAN state the IPC reader folds server answers into and the window renders.
#[derive(Debug, Default)]
pub struct LanChrome {
    /// The approval prompt waiting to be raised on the GPUI foreground. At most
    /// one is held: the server bounds how many approvals may be pending, and a
    /// second request replaces the first rather than stacking modals.
    pending_approval: Option<LanApprovalDialog>,
    /// Peers from the last `LanPeerList`, in the order the server returned them.
    peers: Vec<LanPeerInfo>,
    /// This machine's own LAN environment, once probed.
    env: Option<LanEnvSummary>,
    /// Where this client's own LAN dial has got to.
    dial: LanDialStatus,
}

impl LanChrome {
    /// Empty state: nothing pending, no peers, no environment, no dial.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Park an inbound approval request for the foreground to raise.
    ///
    /// Replaces any prompt not yet taken. Losing an un-raised prompt is safe:
    /// the server holds each connection until its own approval timeout and
    /// reveals nothing in the meantime, so the worst case is that the older
    /// device is refused by timeout rather than by an explicit Decline.
    pub fn park_approval(&mut self, dialog: LanApprovalDialog) {
        self.pending_approval = Some(dialog);
    }

    /// Take the parked approval prompt, if any, so the foreground can own it.
    #[must_use]
    pub fn take_approval(&mut self) -> Option<LanApprovalDialog> {
        self.pending_approval.take()
    }

    /// Whether a prompt is parked and not yet raised.
    #[must_use]
    pub fn approval_pending(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Replace the discovered peer list from a `LanPeerList` reply.
    pub fn set_peers(&mut self, peers: Vec<LanPeerInfo>) {
        self.peers = peers;
    }

    /// The peers from the last `LanPeerList`.
    #[must_use]
    pub fn peers(&self) -> &[LanPeerInfo] {
        &self.peers
    }

    /// How many discovered peers are currently advertised. Evicted-but-remembered
    /// peers are excluded, so this is what a connect affordance would offer.
    #[must_use]
    pub fn online_peer_count(&self) -> usize {
        self.peers.iter().filter(|peer| peer.online).count()
    }

    /// Record this machine's own LAN environment from a `LanEnv` reply.
    pub fn set_env(&mut self, env: LanEnvSummary) {
        self.env = Some(env);
    }

    /// This machine's own LAN environment, once probed.
    #[must_use]
    pub fn env(&self) -> Option<&LanEnvSummary> {
        self.env.as_ref()
    }

    /// Move the dial to "held pending approval on the peer".
    pub fn awaiting_approval(&mut self) {
        self.dial = LanDialStatus::AwaitingApproval;
    }

    /// Settle the dial on a terminal outcome.
    pub fn settle_dial(&mut self, outcome: LanConnectOutcome) {
        self.dial = LanDialStatus::Settled(outcome);
    }

    /// Where this client's own LAN dial has got to.
    #[must_use]
    pub fn dial(&self) -> LanDialStatus {
        self.dial
    }

    /// One line of user-facing LAN status, or `None` while there is nothing to
    /// say (an idle dial on a machine whose LAN surface is dormant).
    ///
    /// The dial state wins when it is not idle: a client that is waiting on — or
    /// was refused by — a peer has nothing more urgent to report.
    #[must_use]
    pub fn status_line(&self) -> Option<String> {
        match self.dial {
            LanDialStatus::AwaitingApproval => {
                return Some(String::from("Waiting for approval on the peer…"));
            }
            LanDialStatus::Settled(outcome) => return Some(dial_outcome_line(outcome)),
            LanDialStatus::Idle => {}
        }
        let env = self.env.as_ref()?;
        if !env.current_network_addable {
            let reason =
                env.current_network_reason.as_deref().unwrap_or("network not identifiable");
            return Some(format!("Local network dormant: {reason}"));
        }
        let online = self.online_peer_count();
        Some(format!("Local network: {online} peer(s)"))
    }
}

/// User-facing copy for a settled LAN dial, one distinct line per typed refusal
/// so a user can tell "you declined me" from "we disagree about versions".
fn dial_outcome_line(outcome: LanConnectOutcome) -> String {
    match outcome {
        LanConnectOutcome::Accepted => String::from("Connected over the local network"),
        LanConnectOutcome::ConnectionFailure => String::from("Local network connection failed"),
        LanConnectOutcome::Refused(reason) => {
            format!("Local network connection refused: {}", refusal_text(reason))
        }
    }
}

/// Short human text for a typed [`LanRefusal`].
fn refusal_text(reason: LanRefusal) -> &'static str {
    match reason {
        LanRefusal::Declined => "the owner declined this device",
        LanRefusal::NotTrustedNetwork => "the peer is not on a trusted network",
        LanRefusal::Disabled => "local network access is off on the peer",
        LanRefusal::IncompatibleVersion => "the peer runs an incompatible version",
        LanRefusal::Busy => "the peer's connection limit was reached",
    }
}

#[cfg(test)]
mod tests;
