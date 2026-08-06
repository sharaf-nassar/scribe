//! Headless tests for the shared LAN chrome state.
//!
//! Cover the three directions the LAN surface reaches the client from: the
//! owning-side approval hand-off between the IPC reader and the GPUI
//! foreground, actionable environment feedback,
//! and the dialing side's pending → settled transition.

use scribe_common::protocol::{LanPeerInfo, LanRefusal};

use super::{LanChrome, LanDialStatus, LanEnvSummary};
use crate::lan_approval::LanApprovalDialog;
use crate::remote::LanConnectOutcome;

fn request(id: u64) -> LanApprovalDialog {
    LanApprovalDialog::new(
        id,
        "colleague-laptop".to_owned(),
        "amber timber pistol".to_owned(),
        "Home".to_owned(),
        false,
    )
}

fn peer(name: &str, online: bool) -> LanPeerInfo {
    LanPeerInfo {
        name: name.to_owned(),
        host: format!("{name}.local"),
        addr: String::from("10.0.0.5"),
        port: 46062,
        protovers: 3,
        online,
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN chrome#Approval hand-off is take-once]]
#[test]
fn parked_approval_is_taken_exactly_once() {
    let mut lan = LanChrome::new();
    assert!(!lan.approval_pending());
    assert!(lan.take_approval().is_none());

    lan.park_approval(request(11));
    assert!(lan.approval_pending());
    assert_eq!(lan.take_approval().map(|d| d.request_id()), Some(11));
    // Taken means gone: the foreground now owns the modal, so a later tick must
    // not raise a second copy of the same prompt.
    assert!(!lan.approval_pending());
    assert!(lan.take_approval().is_none());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN chrome#A second request replaces an unraised one]]
#[test]
fn a_second_request_replaces_an_unraised_prompt() {
    let mut lan = LanChrome::new();
    lan.park_approval(request(1));
    lan.park_approval(request(2));
    assert_eq!(lan.take_approval().map(|d| d.request_id()), Some(2));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN chrome#Status line reports actionable LAN states]]
#[test]
fn status_line_reports_actionable_lan_states() {
    let mut lan = LanChrome::new();
    // Nothing probed yet: no line at all rather than a misleading "0 peers".
    assert!(lan.status_line().is_none());

    lan.set_env(LanEnvSummary {
        device_id_hex: Some("ab".repeat(32)),
        fingerprint_words: Some("amber timber pistol".to_owned()),
        current_network_addable: false,
        current_network_reason: Some("no default gateway".to_owned()),
    });
    let dormant = lan.status_line().expect("a probed environment always says something");
    assert!(dormant.contains("dormant"), "{dormant}");
    assert!(dormant.contains("no default gateway"), "{dormant}");

    lan.set_env(LanEnvSummary { current_network_addable: true, ..LanEnvSummary::default() });
    lan.set_peers(vec![peer("desk", true), peer("old", false), peer("laptop", true)]);
    assert_eq!(lan.online_peer_count(), 2);
    assert_eq!(lan.peers().len(), 3);
    assert!(lan.status_line().is_none());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN chrome#Dial status outranks the environment]]
#[test]
fn dial_status_outranks_the_environment_line() {
    let mut lan = LanChrome::new();
    lan.set_env(LanEnvSummary { current_network_addable: true, ..LanEnvSummary::default() });
    assert_eq!(lan.dial(), LanDialStatus::Idle);

    lan.awaiting_approval();
    assert_eq!(lan.dial(), LanDialStatus::AwaitingApproval);
    assert_eq!(lan.status_line().as_deref(), Some("Waiting for approval on the peer…"));

    lan.settle_dial(LanConnectOutcome::Refused(LanRefusal::Declined));
    assert_eq!(
        lan.dial(),
        LanDialStatus::Settled(LanConnectOutcome::Refused(LanRefusal::Declined))
    );
    let refused = lan.status_line().expect("a refused dial says why");
    assert!(refused.contains("declined"), "{refused}");

    lan.settle_dial(LanConnectOutcome::Accepted);
    assert!(lan.status_line().is_none());
}
