//! Headless tests for the ported LAN device-approval dialog state.
//!
//! Verifies the Decline-default focus, focus cycling, the activation intent, and
//! that the body copy word-wraps the device name, fingerprint, and optional
//! name-collision hint inside the dialog width.

use super::{BODY_WRAP_COLS, LanApprovalAction, LanApprovalDialog};

fn dialog(name_collision: bool) -> LanApprovalDialog {
    LanApprovalDialog::new(
        7,
        "colleague-laptop".to_owned(),
        "amber timber pistol".to_owned(),
        "Home".to_owned(),
        name_collision,
    )
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN device approval]]
#[test]
fn decline_is_the_default_focus() {
    let d = dialog(false);
    assert_eq!(d.confirm(), LanApprovalAction::Decline);
    assert!(!d.approve_focused());
    assert_eq!(d.request_id(), 7);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN device approval]]
#[test]
fn focus_cycles_between_the_two_buttons() {
    let mut d = dialog(false);
    d.focus_next();
    assert_eq!(d.confirm(), LanApprovalAction::Approve);
    assert!(d.approve_focused());
    d.focus_next();
    assert_eq!(d.confirm(), LanApprovalAction::Decline);
    // prev mirrors next for two buttons.
    d.focus_prev();
    assert_eq!(d.confirm(), LanApprovalAction::Approve);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN device approval]]
#[test]
fn body_lists_who_and_fingerprint_and_wraps() {
    let d = dialog(false);
    let lines = d.body_lines();
    assert!(lines.iter().any(|l| l.contains("colleague-laptop")));
    assert!(lines.iter().any(|l| l.contains("Home")));
    assert!(lines.iter().any(|l| l == "Fingerprint:"));
    assert!(lines.iter().any(|l| l.contains("amber")));
    assert!(lines.iter().all(|l| l.chars().count() <= BODY_WRAP_COLS));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI LAN device approval]]
#[test]
fn collision_hint_only_present_when_flagged() {
    assert!(dialog(false).body_lines().iter().all(|l| !l.contains("already trust")));
    assert!(dialog(true).body_lines().iter().any(|l| l.contains("already trust")));
}
