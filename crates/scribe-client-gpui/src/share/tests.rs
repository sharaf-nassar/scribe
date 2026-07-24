//! Headless tests for the ported window-sharing surfaces.
//!
//! Verifies roster-derived roles, participant labels, the control-hint expiry
//! window, and that the control-passing intents lower to the frozen v3
//! `ControlClaim` / `ControlRequest` / `ControlGrant` messages.

use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, ParticipantInfo};

use super::{
    ControlHint, ControlIntent, ControlRequestPrompt, HINT_DURATION, ShareState, participant_label,
};

/// Build a non-local, non-holder participant. Roster-role flags are set by the
/// callers that need them (kept off this helper so it stays within the
/// boolean-parameter budget).
fn participant(id: u64, device: &str, login: &str) -> ParticipantInfo {
    ParticipantInfo {
        participant_id: id,
        device_name: device.to_owned(),
        login_name: login.to_owned(),
        is_local: false,
        is_holder: false,
    }
}

fn shared_state(holder: Option<u64>) -> ShareState {
    let mut owner = participant(1, "this machine", "");
    owner.is_local = true;
    owner.is_holder = holder == Some(1);
    let mut remote = participant(2, "laptop", "alex");
    remote.is_holder = holder == Some(2);
    ShareState {
        window_id: WindowId::new(),
        participants: vec![owner, remote],
        mode: SharingMode::SharedSingleTypist,
        holder,
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn roster_derives_multi_holder_and_labels() {
    let state = shared_state(Some(2));
    assert_eq!(state.participant_count(), 2);
    assert!(state.is_multi());
    assert!(!state.local_is_holder());
    assert_eq!(state.holder_label().as_deref(), Some("laptop (alex)"));

    let local_holder = shared_state(Some(1));
    assert!(local_holder.local_is_holder());
    // Owner's Local entry has an empty login, so the label is the device name.
    assert_eq!(local_holder.holder_label().as_deref(), Some("this machine"));

    let unheld = shared_state(None);
    assert_eq!(unheld.holder_label(), None);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn participant_label_omits_empty_login() {
    assert_eq!(participant_label(&participant(3, "box", "")), "box");
    assert_eq!(participant_label(&participant(3, "box", "sam")), "box (sam)");
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn fresh_hint_is_not_expired_and_keeps_text() {
    let hint = ControlHint::new("laptop (alex) has control".to_owned());
    assert!(!hint.is_expired());
    assert!(hint.text().contains("has control"));
    assert!(hint.expires_at() > std::time::Instant::now());
    assert!(hint.expires_at() <= std::time::Instant::now() + HINT_DURATION);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn viewer_take_control_lowers_to_claim() {
    let state = shared_state(Some(2));
    let intent = state.take_control_intent();
    assert_eq!(intent, ControlIntent::Claim { window_id: state.window_id });
    assert!(matches!(intent.into_message(), ClientMessage::ControlClaim { .. }));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn request_prompt_answer_lowers_to_grant() {
    let window_id = WindowId::new();
    let requester = participant(2, "laptop", "alex");
    let prompt = ControlRequestPrompt::new(window_id, &requester);
    assert_eq!(prompt.window_id(), window_id);
    assert_eq!(prompt.requester_id(), 2);
    assert!(prompt.headline().contains("laptop (alex) wants control"));

    let grant = prompt.answer(true);
    assert_eq!(grant, ControlIntent::Grant { window_id, participant_id: 2, accept: true });
    match grant.into_message() {
        ClientMessage::ControlGrant { participant_id, accept, .. } => {
            assert_eq!(participant_id, 2);
            assert!(accept);
        }
        other => panic!("expected ControlGrant, got {other:?}"),
    }

    let deny = prompt.answer(false);
    assert_eq!(deny, ControlIntent::Grant { window_id, participant_id: 2, accept: false });
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing]]
#[test]
fn request_intent_lowers_to_control_request() {
    let window_id = WindowId::new();
    let intent = ControlIntent::Request { window_id };
    assert!(matches!(intent.into_message(), ClientMessage::ControlRequest { .. }));
}
