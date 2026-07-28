//! Scripted handshake + dial-target tests for the ported remote preamble.
//!
//! The handshake tests drive [`perform_remote_handshake`] over an in-memory
//! `tokio::io::duplex` pair against a scripted fake server, asserting the frozen
//! [`RemoteHandshake`](scribe_common::protocol::ClientMessage::RemoteHandshake) /
//! [`RemoteHandshakeReply`](scribe_common::protocol::ServerMessage::RemoteHandshakeReply)
//! exchange maps to the right [`RemoteConnectOutcome`]. The parser tests cover the
//! `SCRIBE_REMOTE_DIAL` / `SCRIBE_LAN_DIAL` / `SCRIBE_REMOTE_WINDOW` plumbing-hook
//! grammar without mutating process env.

use scribe_common::framing::{read_message, write_message};
use scribe_common::ids::WindowId;
use scribe_common::protocol::{
    ClientMessage, REMOTE_PROTOCOL_VERSION, RemoteRefusal, ServerMessage,
};
use tokio::io::DuplexStream;

use super::{flag_is_set, parse_dial_target, parse_window_target, perform_remote_handshake};
use crate::remote::RemoteConnectOutcome;

/// Read the client's preamble off the server end, assert it is a well-formed
/// `RemoteHandshake`, then write `reply` back so `perform_remote_handshake`
/// resolves. Returns the dialer's advertised device name for assertions.
async fn scripted_server(mut server: DuplexStream, reply: ServerMessage) -> String {
    let preamble = read_message::<ClientMessage, _>(&mut server).await.unwrap();
    let ClientMessage::RemoteHandshake { remote_protocol_version, device_name, .. } = preamble
    else {
        panic!("first frame was not RemoteHandshake");
    };
    assert_eq!(remote_protocol_version, REMOTE_PROTOCOL_VERSION);
    write_message(&mut server, &reply).await.unwrap();
    device_name
}

fn accepted_reply() -> ServerMessage {
    ServerMessage::RemoteHandshakeReply {
        accepted: true,
        refusal: None,
        server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        server_scribe_version: "test".to_owned(),
    }
}

fn refused_reply(refusal: Option<RemoteRefusal>) -> ServerMessage {
    ServerMessage::RemoteHandshakeReply {
        accepted: false,
        refusal,
        server_remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        server_scribe_version: "test".to_owned(),
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[tokio::test]
async fn accepted_preamble_yields_accepted_outcome() {
    let (mut client, server) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(scripted_server(server, accepted_reply()));
    let outcome = perform_remote_handshake(&mut client, "my-laptop".to_owned()).await;
    assert_eq!(outcome, RemoteConnectOutcome::Accepted);
    assert_eq!(server_task.await.unwrap(), "my-laptop");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[tokio::test]
async fn typed_refusal_is_propagated() {
    let (mut client, server) = tokio::io::duplex(4096);
    let reply = refused_reply(Some(RemoteRefusal::IncompatibleVersion));
    let server_task = tokio::spawn(scripted_server(server, reply));
    let outcome = perform_remote_handshake(&mut client, "my-laptop".to_owned()).await;
    assert_eq!(outcome, RemoteConnectOutcome::Refused(RemoteRefusal::IncompatibleVersion));
    server_task.await.unwrap();
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[tokio::test]
async fn refusal_without_reason_is_connection_failure() {
    let (mut client, server) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(scripted_server(server, refused_reply(None)));
    let outcome = perform_remote_handshake(&mut client, "my-laptop".to_owned()).await;
    assert_eq!(outcome, RemoteConnectOutcome::ConnectionFailure);
    server_task.await.unwrap();
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[tokio::test]
async fn unexpected_first_frame_is_connection_failure() {
    let (mut client, mut server) = tokio::io::duplex(4096);
    let server_task = tokio::spawn(async move {
        let _preamble = read_message::<ClientMessage, _>(&mut server).await.unwrap();
        // Answer with a frame that is NOT the mandatory reply.
        write_message(&mut server, &ServerMessage::QuitRequested).await.unwrap();
    });
    let outcome = perform_remote_handshake(&mut client, "my-laptop".to_owned()).await;
    assert_eq!(outcome, RemoteConnectOutcome::ConnectionFailure);
    server_task.await.unwrap();
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[tokio::test]
async fn peer_closing_before_reply_is_connection_failure() {
    let (mut client, server) = tokio::io::duplex(4096);
    // Drop the server end immediately so the read half hits EOF.
    drop(server);
    let outcome = perform_remote_handshake(&mut client, "my-laptop".to_owned()).await;
    assert_eq!(outcome, RemoteConnectOutcome::ConnectionFailure);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[test]
fn dial_target_grammar() {
    assert_eq!(parse_dial_target("  ", 46061), None);
    assert_eq!(parse_dial_target("host", 46061), Some(("host".to_owned(), 46061)));
    assert_eq!(parse_dial_target("host:1234", 46061), Some(("host".to_owned(), 1234)));
    // Bad port falls back to the default.
    assert_eq!(parse_dial_target("host:bad", 46061), Some(("host".to_owned(), 46061)));
    // Bare IPv6 literal keeps its colons and dials on the default port.
    assert_eq!(parse_dial_target("::1", 46061), Some(("::1".to_owned(), 46061)));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote handshake]]
#[test]
fn window_target_and_takeover_flag_grammar() {
    assert_eq!(parse_window_target("  "), None);
    assert_eq!(parse_window_target("not-a-uuid"), None);
    let id = WindowId::new();
    assert_eq!(parse_window_target(&id.to_full_string()), Some(id));

    assert!(flag_is_set("1"));
    assert!(flag_is_set(" true "));
    assert!(flag_is_set("TRUE"));
    assert!(!flag_is_set("0"));
    assert!(!flag_is_set(""));
}
