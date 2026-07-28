//! Headless tests for the LAN dial preamble.
//!
//! The TCP + mutual-TLS half needs a real peer and is covered by the scripted
//! E2E; what is testable without a display server or a second machine is the
//! framed preamble itself, so these drive [`handshake`] over an in-memory duplex
//! exactly as the ported tailnet handshake test does.

use scribe_common::framing::{read_message, write_message};
use scribe_common::protocol::{ClientMessage, LanRefusal, REMOTE_PROTOCOL_VERSION, ServerMessage};
use tokio::io::duplex;

use super::handshake;
use crate::remote::LanConnectOutcome;

/// Read the client's preamble off the peer end and assert it is the `LanHello`
/// the owning side gates on, returning the advertised device name.
async fn expect_lan_hello<S>(peer: &mut S) -> String
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match read_message::<ClientMessage, _>(peer).await.expect("preamble") {
        ClientMessage::LanHello { device_name, remote_protocol_version } => {
            assert_eq!(remote_protocol_version, REMOTE_PROTOCOL_VERSION);
            device_name
        }
        other => panic!("expected LanHello, got {other:?}"),
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN dial#Trusted device is admitted without a pending frame]]
#[tokio::test]
async fn a_trusted_device_is_admitted_without_a_pending_frame() {
    let (mut client, mut peer) = duplex(4096);
    let server = tokio::spawn(async move {
        let name = expect_lan_hello(&mut peer).await;
        write_message(
            &mut peer,
            &ServerMessage::LanApprovalResult { approved: true, refusal: None },
        )
        .await
        .expect("result");
        name
    });

    let mut pending_reports = 0_u32;
    let outcome = handshake(&mut client, "desk-mini".to_owned(), || pending_reports += 1).await;

    assert_eq!(outcome, LanConnectOutcome::Accepted);
    assert_eq!(pending_reports, 0, "a trusted device must never raise the waiting state");
    assert_eq!(server.await.expect("peer task"), "desk-mini");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN dial#Unknown device waits then settles]]
#[tokio::test]
async fn an_unknown_device_reports_waiting_then_settles_on_the_decision() {
    let (mut client, mut peer) = duplex(4096);
    tokio::spawn(async move {
        expect_lan_hello(&mut peer).await;
        write_message(&mut peer, &ServerMessage::LanApprovalPending).await.expect("pending");
        write_message(
            &mut peer,
            &ServerMessage::LanApprovalResult {
                approved: false,
                refusal: Some(LanRefusal::Declined),
            },
        )
        .await
        .expect("result");
    });

    let mut pending_reports = 0_u32;
    let outcome = handshake(&mut client, "desk-mini".to_owned(), || pending_reports += 1).await;

    assert_eq!(outcome, LanConnectOutcome::Refused(LanRefusal::Declined));
    assert_eq!(pending_reports, 1, "the waiting state must be reported exactly once");
}

/// Run the preamble against a peer that answers `reply` (or hangs up when it is
/// `None`) and report the outcome the dialer settled on.
async fn outcome_against(reply: Option<ServerMessage>) -> LanConnectOutcome {
    let (mut client, mut peer) = duplex(4096);
    tokio::spawn(async move {
        expect_lan_hello(&mut peer).await;
        if let Some(reply) = reply {
            write_message(&mut peer, &reply).await.expect("reply");
        }
        drop(peer);
    });
    handshake(&mut client, "desk-mini".to_owned(), || {}).await
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN dial#Malformed gate answers fail closed]]
#[tokio::test]
async fn a_reasonless_refusal_or_an_unexpected_frame_fails_closed() {
    // A refusal with no reason is a protocol violation, not a cause to invent.
    let reasonless = ServerMessage::LanApprovalResult { approved: false, refusal: None };
    assert_eq!(outcome_against(Some(reasonless)).await, LanConnectOutcome::ConnectionFailure);

    // A peer that answers something other than the gate is equally a failure:
    // no window data may be trusted from a connection that skipped the gate.
    assert_eq!(
        outcome_against(Some(ServerMessage::QuitRequested)).await,
        LanConnectOutcome::ConnectionFailure
    );

    // And so is a peer that hangs up before answering at all.
    assert_eq!(outcome_against(None).await, LanConnectOutcome::ConnectionFailure);
}
