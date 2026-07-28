//! Headless tests for the ported window-sharing surfaces.
//!
//! Verifies roster-derived roles, participant labels, the control-hint expiry
//! window, and that the control-passing intents lower to the frozen v3
//! `ControlClaim` / `ControlRequest` / `ControlGrant` messages.

use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::{ClientMessage, ParticipantInfo, ShareEndReason};

use super::{
    ControlHint, ControlIntent, ControlRequestPrompt, HINT_DURATION, ShareChrome, ShareKey,
    ShareKeyOutcome, ShareState, participant_label,
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

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing#Roster drives the presence surfaces]]
#[test]
fn roster_drives_presence_and_drains_to_solo() {
    let mut chrome = ShareChrome::new();
    assert!(chrome.presence().is_none());

    chrome.apply_roster(shared_state(Some(2)));
    let presence = chrome.presence().expect("a multi-participant roster raises the badge");
    assert_eq!(presence.participant_count, 2);
    assert_eq!(presence.holder.as_deref(), Some("laptop (alex)"));

    let rows = shared_state(Some(2)).roster_rows();
    // The owner's own entry is already named "this machine" server-side, so the
    // row does not repeat the marker; a differently named local entry gets it.
    assert_eq!(rows[0].text(), "this machine");
    assert_eq!(rows[1].text(), "laptop (alex) \u{00B7} has control");
    let mut renamed = shared_state(Some(1));
    renamed.participants[0].device_name = "desktop".to_owned();
    assert_eq!(
        renamed.roster_rows()[0].text(),
        "desktop \u{00B7} this machine \u{00B7} has control"
    );

    // The share drained back to just the owner: no badge, no viewer affordances.
    let mut solo = shared_state(Some(1));
    solo.participants.truncate(1);
    chrome.apply_roster(solo);
    assert!(chrome.presence().is_none());
    assert!(!chrome.is_viewer());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing#Viewer keystrokes claim control]]
#[test]
fn viewer_keystroke_hints_then_claims_control() {
    let state = shared_state(Some(2));
    let window_id = state.window_id;
    let mut chrome = ShareChrome::new();
    chrome.apply_roster(state);
    assert!(chrome.is_viewer());

    // The first keystroke is swallowed and raises the take-control hint.
    assert_eq!(chrome.intercept_key(ShareKey::Other), ShareKeyOutcome::Suppressed);
    // Enter while the hint is up claims control on the wire.
    assert_eq!(
        chrome.intercept_key(ShareKey::Enter),
        ShareKeyOutcome::Emit(ControlIntent::Claim { window_id })
    );

    // Once this machine holds control again, keys reach the terminal untouched.
    chrome.apply_roster(shared_state(Some(1)));
    assert!(!chrome.is_viewer());
    assert_eq!(chrome.intercept_key(ShareKey::Other), ShareKeyOutcome::Passthrough);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing#Prompt is modal until answered]]
#[test]
fn control_request_prompt_is_modal_until_answered() {
    let state = shared_state(Some(1));
    let window_id = state.window_id;
    let mut chrome = ShareChrome::new();
    chrome.apply_roster(state);

    let requester = participant(2, "laptop", "alex");
    chrome.request(ControlRequestPrompt::new(window_id, &requester));
    assert!(chrome.has_prompt());
    // Every other key is swallowed while the decision is open.
    assert_eq!(chrome.intercept_key(ShareKey::Other), ShareKeyOutcome::Suppressed);
    assert!(chrome.has_prompt());

    let granted = ControlIntent::Grant { window_id, participant_id: 2, accept: true };
    assert_eq!(chrome.intercept_key(ShareKey::Enter), ShareKeyOutcome::Emit(granted));
    assert!(!chrome.has_prompt());

    // Esc denies the next request instead.
    chrome.request(ControlRequestPrompt::new(window_id, &requester));
    let denied = ControlIntent::Grant { window_id, participant_id: 2, accept: false };
    assert_eq!(chrome.intercept_key(ShareKey::Escape), ShareKeyOutcome::Emit(denied));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing#Denied and ended notices]]
#[test]
fn denied_and_ended_leave_a_transient_notice() {
    let mut chrome = ShareChrome::new();
    chrome.apply_roster(shared_state(Some(2)));

    chrome.deny();
    assert!(chrome.presence().is_some(), "a denial leaves the share itself intact");

    chrome.end(ShareEndReason::OwnerClosed);
    assert!(chrome.presence().is_none());
    assert!(!chrome.is_viewer());
    assert!(!chrome.has_prompt());
    // The notice is transient, not sticky, but has not aged out yet.
    assert!(!chrome.expire_hint());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI window sharing#Self id resolves the local seat]]
#[test]
fn welcome_participant_id_resolves_the_local_seat() {
    // The roster marks participant 1 as the owner's `is_local` entry, but this
    // connection is seated as participant 2 — the id from `Welcome` wins, so the
    // client reads itself as the holder rather than as a viewer.
    let mut chrome = ShareChrome::new();
    chrome.set_self_id(Some(2));
    chrome.apply_roster(shared_state(Some(2)));
    assert!(!chrome.is_viewer());

    chrome.apply_roster(shared_state(Some(1)));
    assert!(chrome.is_viewer());
}
