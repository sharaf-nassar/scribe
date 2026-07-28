//! Headless tests for the ported displaced-client "lost control" state.
//!
//! Verifies the banner headline copy and the Enter-only reclaim gate that keeps
//! every other key suppressed while the frozen frame is shown.

use super::{LostControlState, ReclaimKey};

// @lat: [[test#GPUI Client Headless Suites#GPUI lost control banner]]
#[test]
fn headline_names_controller_and_account() {
    let state = LostControlState::new("workstation".to_owned(), "alex".to_owned());
    assert_eq!(state.headline(), "Controlled by workstation (alex)");
    assert!(!LostControlState::hint().is_empty());
}

// @lat: [[test#GPUI Client Headless Suites#GPUI lost control banner]]
#[test]
fn only_enter_reclaims_control() {
    assert!(LostControlState::reclaim_requested(ReclaimKey::Enter));
    assert!(!LostControlState::reclaim_requested(ReclaimKey::Other));
    // The live key path lowers a GPUI keystroke name here, so the same
    // Enter-only rule has to survive that translation.
    assert_eq!(ReclaimKey::from_keystroke("enter"), ReclaimKey::Enter);
    assert_eq!(ReclaimKey::from_keystroke("escape"), ReclaimKey::Other);
    assert_eq!(ReclaimKey::from_keystroke("q"), ReclaimKey::Other);
}
