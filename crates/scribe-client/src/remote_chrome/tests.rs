//! Headless tests for the shared tailnet chrome state.
//!
//! Cover the four directions the remote surface reaches the client from: the
//! actionable environment feedback, the dialing
//! side's settled outcome, the displaced-client freeze a `WindowTakenOver`
//! raises, and the bounded automation queue a `RunAction` lands in.

use scribe_common::protocol::{AutomationAction, RemotePeerInfo, RemoteRefusal};

use super::{RemoteChrome, RemoteDialStatus, RemoteEnvSummary};
use crate::lost_control::LostControlState;
use crate::remote::RemoteConnectOutcome;

fn peer(name: &str, online: bool) -> RemotePeerInfo {
    RemotePeerInfo { name: name.to_owned(), addr: format!("100.64.0.{}", name.len()), online }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote chrome#Status line reports actionable tailnet states]]
#[test]
fn status_line_reports_actionable_tailnet_states() {
    let mut remote = RemoteChrome::new();
    // Nothing probed yet: no line at all rather than a misleading "0 peers".
    assert!(remote.status_line().is_none());

    remote.set_env(RemoteEnvSummary { account: None, tailscale_detected: false });
    assert_eq!(remote.status_line().as_deref(), Some("Tailscale not detected"));

    remote.set_env(RemoteEnvSummary {
        account: Some("user@example.com".to_owned()),
        tailscale_detected: true,
    });
    remote.set_peers(vec![peer("desk", true), peer("old", false), peer("laptop", true)]);
    assert_eq!(remote.online_peer_count(), 2);
    assert_eq!(remote.peers().len(), 3);
    assert!(remote.status_line().is_none());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote chrome#Dial and severance outrank the environment]]
#[test]
fn dial_and_severance_outrank_the_environment_line() {
    let mut remote = RemoteChrome::new();
    remote.set_env(RemoteEnvSummary {
        account: Some("user@example.com".to_owned()),
        tailscale_detected: true,
    });
    assert_eq!(remote.dial(), RemoteDialStatus::Idle);
    assert!(remote.transport_label().is_none());

    remote.settle_dial(RemoteConnectOutcome::Refused(RemoteRefusal::Unauthorized));
    let refused = remote.status_line().expect("a refused dial says why");
    assert!(refused.contains("not authorized"), "{refused}");
    // A refused dial is not a transport this window reached anything over.
    assert!(remote.transport_label().is_none());

    remote.settle_dial(RemoteConnectOutcome::Accepted);
    assert!(remote.status_line().is_none());
    assert_eq!(remote.transport_label(), Some("Tailscale"));

    // A severed link is more urgent than the dial that established it.
    remote.sever(RemoteRefusal::Disabled);
    assert_eq!(remote.severed(), Some(RemoteRefusal::Disabled));
    let severed = remote.status_line().expect("a severed link always says something");
    assert!(severed.contains("remote access is off"), "{severed}");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote chrome#Displacement freezes and reclaims once]]
#[test]
fn displacement_freezes_and_reclaims_exactly_once() {
    let mut remote = RemoteChrome::new();
    assert!(remote.displaced().is_none());
    // Nothing to reclaim: the key path must not put a claim on the wire for a
    // banner that was never up.
    assert!(!remote.reclaim());

    remote.sever(RemoteRefusal::Busy);
    remote.displace(LostControlState::new("desk-mini".to_owned(), "user@example.com".to_owned()));
    assert_eq!(
        remote.displaced().map(LostControlState::headline).as_deref(),
        Some("Controlled by desk-mini (user@example.com)")
    );
    // Displacement has its own banner; the status bar keeps the severance reason.
    assert!(remote.status_line().is_some_and(|line| line.contains("connection limit")));

    assert!(remote.reclaim());
    assert!(remote.displaced().is_none());
    assert!(!remote.reclaim());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote chrome#Automation queue is bounded and FIFO]]
#[test]
fn automation_queue_is_bounded_and_fifo() {
    let mut remote = RemoteChrome::new();
    assert!(remote.take_action().is_none());

    remote.queue_action(AutomationAction::NewTab);
    remote.queue_action(AutomationAction::SplitVertical);
    assert!(matches!(remote.take_action(), Some(AutomationAction::NewTab)));
    assert!(matches!(remote.take_action(), Some(AutomationAction::SplitVertical)));
    assert!(remote.take_action().is_none());

    // Overflow drops the OLDEST: the newest request is the one the user just
    // typed, so a wedged window must not replay a minute of stale actions.
    for _ in 0..super::MAX_QUEUED_ACTIONS {
        remote.queue_action(AutomationAction::NewTab);
    }
    remote.queue_action(AutomationAction::CloseTab);
    let drained: Vec<_> = std::iter::from_fn(|| remote.take_action()).collect();
    assert_eq!(drained.len(), super::MAX_QUEUED_ACTIONS);
    assert!(matches!(drained.last(), Some(AutomationAction::CloseTab)));
}
