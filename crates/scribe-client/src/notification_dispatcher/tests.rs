//! Unit coverage for the platform-independent notification dispatcher logic:
//! the `replaces_id` coalescing state machine and the freedesktop
//! `expire_timeout` mapping. The zbus transport and click-to-focus wiring are
//! verified by the manual parity checklist (notification click-to-focus).

use scribe_common::config::NotifyTimeoutMode;
use scribe_common::ids::SessionId;

use super::*;

// @lat: [[test#GPUI Notification Dispatcher#Timeout mode maps to expire_timeout]]
#[test]
fn expire_timeout_maps_each_mode() {
    assert_eq!(expire_timeout_millis(NotifyTimeoutMode::SystemDefault, 5), -1);
    assert_eq!(expire_timeout_millis(NotifyTimeoutMode::Never, 5), 0);
    assert_eq!(expire_timeout_millis(NotifyTimeoutMode::Custom, 3), 3000);
    // Saturates instead of overflowing on an absurd custom timeout.
    assert_eq!(expire_timeout_millis(NotifyTimeoutMode::Custom, u32::MAX), i32::MAX);
}

// @lat: [[test#GPUI Notification Dispatcher#Same session reuses replaces_id]]
#[test]
fn same_session_reuses_replaces_id_for_one_toast() {
    let mut state = NotifState::new();
    let session = SessionId::new();
    // First show: no prior toast, daemon assigns id 10.
    assert_eq!(state.replaces_for(session), 0);
    state.record_shown(session, 0, 10);
    // Second show reuses id 10 as replaces; daemon swaps in place, returns 10.
    assert_eq!(state.replaces_for(session), 10);
    state.record_shown(session, 10, 10);
    assert_eq!(state.session_for_id(10), Some(session));
    // Still exactly one live toast.
    assert_eq!(state.live_ids(), vec![10]);
}

// @lat: [[test#GPUI Notification Dispatcher#Expired toast reallocation drops stale mapping]]
#[test]
fn expired_toast_reallocation_drops_stale_reverse_mapping() {
    let mut state = NotifState::new();
    let session = SessionId::new();
    state.record_shown(session, 0, 10);
    // The prior toast expired; passing replaces=10 the daemon allocates 11.
    state.record_shown(session, 10, 11);
    assert_eq!(state.session_for_id(10), None);
    assert_eq!(state.session_for_id(11), Some(session));
    assert_eq!(state.replaces_for(session), 11);
    assert_eq!(state.live_ids(), vec![11]);
}

// @lat: [[test#GPUI Notification Dispatcher#Session close removes both mappings]]
#[test]
fn take_session_removes_both_mappings() {
    let mut state = NotifState::new();
    let session = SessionId::new();
    state.record_shown(session, 0, 10);
    assert_eq!(state.take_session(session), Some(10));
    assert_eq!(state.take_session(session), None);
    assert_eq!(state.session_for_id(10), None);
    assert_eq!(state.replaces_for(session), 0);
}

// @lat: [[test#GPUI Notification Dispatcher#Daemon closed signal clears mappings]]
#[test]
fn on_closed_clears_both_mappings() {
    let mut state = NotifState::new();
    let session = SessionId::new();
    state.record_shown(session, 0, 10);
    state.on_closed(10);
    assert_eq!(state.session_for_id(10), None);
    assert_eq!(state.replaces_for(session), 0);
    // Closing an unknown id is a no-op.
    state.on_closed(999);
}

// @lat: [[test#GPUI Notification Dispatcher#Shutdown closes every live toast]]
#[test]
fn live_ids_and_clear_cover_shutdown() {
    let mut state = NotifState::new();
    let a = SessionId::new();
    let b = SessionId::new();
    state.record_shown(a, 0, 10);
    state.record_shown(b, 0, 20);
    let mut ids = state.live_ids();
    ids.sort_unstable();
    assert_eq!(ids, vec![10, 20]);
    state.clear();
    assert!(state.live_ids().is_empty());
    assert_eq!(state.replaces_for(a), 0);
    assert_eq!(state.replaces_for(b), 0);
}
