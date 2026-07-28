//! Headless tests for the ported remote-connect picker + reconnect overlay.
//!
//! Verifies the merge/dedup/sort of the peer list, the step transitions and
//! typed intents, the failure-copy mapping, the awaiting-approval swap, and the
//! reconnect overlay's key/click actions — all without a display server.

use scribe_common::config::SharingMode;
use scribe_common::ids::WindowId;
use scribe_common::protocol::{
    LanPeerInfo, LanRefusal, REMOTE_PROTOCOL_VERSION, RemotePeerInfo, RemoteRefusal, WindowInfo,
};

use super::{
    LanConnectOutcome, PeerTransport, PickerKey, ReconnectAction, ReconnectOverlay, RemoteConnect,
    RemoteConnectAction, RemoteConnectOutcome,
};

fn tailnet_peer(name: &str, online: bool) -> RemotePeerInfo {
    RemotePeerInfo { name: name.to_owned(), addr: format!("{name}.ts.net"), online }
}

fn lan_peer(name: &str, host: &str, online: bool) -> LanPeerInfo {
    LanPeerInfo {
        name: name.to_owned(),
        host: host.to_owned(),
        addr: format!("192.168.1.{}", host.len()),
        port: 46062,
        protovers: REMOTE_PROTOCOL_VERSION,
        online,
    }
}

fn window(session_count: usize, connected: bool, participant_count: usize) -> WindowInfo {
    WindowInfo {
        window_id: WindowId::new(),
        session_count,
        connected,
        workspace_names: Vec::new(),
        controller: None,
        participants: Vec::new(),
        mode: if participant_count >= 2 { Some(SharingMode::SharedSingleTypist) } else { None },
        participant_count,
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn dual_reachable_machine_collapses_to_one_lan_row() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_peers(vec![tailnet_peer("alpha", true), tailnet_peer("beta", true)]);
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);

    let view = picker.view();
    // alpha (LAN, dual-reachable) + beta (tailnet) = 2 rows, not 3.
    assert_eq!(view.rows.len(), 2);
    let alpha = &view.rows[0];
    assert!(alpha.text.contains("alpha"));
    assert!(alpha.text.contains("Local network"));
    assert!(alpha.text.contains("(also Tailscale)"));
    let beta = &view.rows[1];
    assert!(beta.text.contains("beta"));
    assert!(beta.text.contains("Tailscale"));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn incompatible_lan_peer_is_dropped_before_offering() {
    let mut picker = RemoteConnect::new();
    picker.open();
    let mut bad = lan_peer("gamma", "gamma", true);
    bad.protovers = REMOTE_PROTOCOL_VERSION + 1;
    picker.set_lan_peers(vec![bad]);
    // Only the placeholder "No devices found" row remains.
    let view = picker.view();
    assert_eq!(view.rows.len(), 1);
    assert!(view.rows[0].dim);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn online_peers_sort_before_offline() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_peers(vec![tailnet_peer("zeta", false), tailnet_peer("yankee", true)]);
    let view = picker.view();
    assert!(view.rows[0].text.contains("yankee"));
    assert!(view.rows[1].text.contains("zeta"));
    assert!(view.rows[1].dim); // offline is dimmed
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn enter_on_online_peer_probes_windows_over_its_transport() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    match picker.handle_key(PickerKey::Enter) {
        RemoteConnectAction::ProbeWindows { transport, .. } => {
            assert_eq!(transport, PeerTransport::Lan);
        }
        other => panic!("expected ProbeWindows, got {other:?}"),
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn manual_entry_wins_over_highlighted_peer_and_dials_tailnet() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    for ch in "host.example:1234".chars() {
        picker.handle_key(PickerKey::Char(ch));
    }
    match picker.handle_key(PickerKey::Enter) {
        RemoteConnectAction::ProbeWindows { host, port, transport } => {
            assert_eq!(host, "host.example");
            assert_eq!(port, 1234);
            assert_eq!(transport, PeerTransport::Tailnet);
        }
        other => panic!("expected ProbeWindows, got {other:?}"),
    }
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn window_step_enter_attaches_or_creates() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    picker.handle_key(PickerKey::Enter); // -> windows stage, loading
    // Enter while loading is a no-op.
    assert_eq!(picker.handle_key(PickerKey::Enter), RemoteConnectAction::None);

    let existing = window(2, false, 0);
    picker.set_windows("192.168.1.5", 46062, vec![existing]);
    // First row is the existing window: Attach.
    match picker.handle_key(PickerKey::Enter) {
        RemoteConnectAction::Attach { transport, .. } => {
            assert_eq!(transport, PeerTransport::Lan);
        }
        other => panic!("expected Attach, got {other:?}"),
    }
    // Move to the trailing "New window" row: NewWindow.
    picker.handle_key(PickerKey::Down);
    assert!(matches!(picker.handle_key(PickerKey::Enter), RemoteConnectAction::NewWindow { .. }));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn shared_window_row_shows_occupancy() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    picker.handle_key(PickerKey::Enter);
    picker.set_windows("192.168.1.5", 46062, vec![window(1, true, 3)]);
    let view = picker.view();
    assert!(view.rows[0].text.contains("3 attached"));
    assert!(view.rows[0].text.contains("shared"));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn refused_and_failed_dials_map_to_typed_copy() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_peers(vec![tailnet_peer("alpha", true)]);
    picker.handle_key(PickerKey::Enter);
    picker.on_dial_outcome(RemoteConnectOutcome::Refused(RemoteRefusal::Disabled));
    let view = picker.view();
    assert_eq!(view.title, "Couldn't connect");
    assert!(view.rows.iter().any(|r| r.text.contains("Remote access is turned off")));

    // Esc/Enter from the failed step returns to the peer list.
    assert_eq!(picker.handle_key(PickerKey::Enter), RemoteConnectAction::RequestPeers);
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn lan_declined_maps_to_lan_copy() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    picker.handle_key(PickerKey::Enter);
    picker.on_lan_dial_outcome(LanConnectOutcome::Refused(LanRefusal::Declined));
    assert!(picker.view().rows.iter().any(|r| r.text.contains("declined this device")));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn awaiting_approval_swaps_loading_note_until_settled() {
    let mut picker = RemoteConnect::new();
    picker.open();
    picker.set_lan_peers(vec![lan_peer("alpha", "alpha", true)]);
    picker.handle_key(PickerKey::Enter);
    picker.on_awaiting_approval();
    assert!(picker.view().title.starts_with("Waiting for approval"));
    // Window list arriving clears the overlay.
    picker.set_windows("192.168.1.5", 46062, vec![window(1, false, 0)]);
    assert!(picker.view().title.starts_with("Windows on"));
}

// @lat: [[test#GPUI Client Headless Suites#GPUI remote connect picker]]
#[test]
fn reconnect_overlay_key_and_click_actions() {
    let mut overlay = ReconnectOverlay::reconnecting("alpha".to_owned(), 1);
    assert_eq!(overlay.key_action(PickerKey::Escape), ReconnectAction::Cancel);
    assert_eq!(overlay.click_action(), ReconnectAction::None);
    overlay.set_attempt(4);
    assert!(overlay.view().rows[0].text.contains("Attempt 4"));

    let settled = ReconnectOverlay::settled_unreachable("alpha".to_owned());
    assert!(settled.is_settled());
    assert_eq!(settled.key_action(PickerKey::Enter), ReconnectAction::Reconnect);
    assert_eq!(settled.key_action(PickerKey::Escape), ReconnectAction::Close);
    assert_eq!(settled.click_action(), ReconnectAction::Reconnect);
}
