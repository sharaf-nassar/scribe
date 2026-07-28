//! Unit coverage for the notification decision gate: the `Processing →
//! attention` transition rule, the three focus conditions, and the
//! focus-on-activate fallback. All pure — the zbus delivery half lives in
//! [`crate::notification_dispatcher`] and is proven by the scripted E2E.

use scribe_common::config::{NotificationsConfig, NotifyCondition};

use super::*;

fn center(condition: NotifyCondition, enabled: bool) -> NotificationCenter {
    NotificationCenter::new(NotificationsConfig { enabled, condition, ..Default::default() })
}

// @lat: [[test#GPUI Notification Gate#Processing to attention fires once]]
#[test]
fn processing_to_attention_fires_and_does_not_repeat() {
    let mut center = center(NotifyCondition::WhenUnfocused, true);
    let session = SessionId::new();
    // A first observed state never fires: nothing was processing before it.
    assert_eq!(center.on_ai_state_changed(session, &AiState::IdlePrompt), None);
    assert_eq!(center.on_ai_state_changed(session, &AiState::Processing), None);
    assert_eq!(
        center.on_ai_state_changed(session, &AiState::IdlePrompt),
        Some(NotificationPayload { session_id: session, state: AiState::IdlePrompt })
    );
    // Sitting in the same attention state is not a new transition.
    assert_eq!(center.on_ai_state_changed(session, &AiState::IdlePrompt), None);
}

// @lat: [[test#GPUI Notification Gate#Non-attention states never fire]]
#[test]
fn non_attention_states_never_fire() {
    let mut center = center(NotifyCondition::Always, true);
    let session = SessionId::new();
    assert_eq!(center.on_ai_state_changed(session, &AiState::Processing), None);
    assert_eq!(center.on_ai_state_changed(session, &AiState::Error), None);
    assert_eq!(center.on_ai_state_changed(session, &AiState::Processing), None);
    assert!(center.on_ai_state_changed(session, &AiState::PermissionPrompt).is_some());
}

// @lat: [[test#GPUI Notification Gate#Disabled notifications never fire]]
#[test]
fn disabled_config_tracks_state_but_never_fires() {
    let mut center = center(NotifyCondition::Always, false);
    let session = SessionId::new();
    assert_eq!(center.on_ai_state_changed(session, &AiState::Processing), None);
    assert_eq!(center.on_ai_state_changed(session, &AiState::IdlePrompt), None);
    // Re-enabling picks the machine up where it left off rather than needing a
    // fresh Processing cycle to re-seed.
    center.reconfigure(NotificationsConfig { enabled: true, ..Default::default() });
    assert_eq!(center.on_ai_state_changed(session, &AiState::Processing), None);
    assert!(center.on_ai_state_changed(session, &AiState::IdlePrompt).is_some());
}

// @lat: [[test#GPUI Notification Gate#Focus conditions gate delivery]]
#[test]
fn focus_conditions_gate_delivery() {
    let unfocused = center(NotifyCondition::WhenUnfocused, true);
    assert!(unfocused.suppresses(FocusPosition::BackgroundTab));
    assert!(unfocused.suppresses(FocusPosition::Foreground));
    assert!(!unfocused.suppresses(FocusPosition::WindowUnfocused));

    let background = center(NotifyCondition::WhenUnfocusedOrBackgroundTab, true);
    // Focused window but a background tab still notifies.
    assert!(!background.suppresses(FocusPosition::BackgroundTab));
    assert!(background.suppresses(FocusPosition::Foreground));
    assert!(!background.suppresses(FocusPosition::WindowUnfocused));

    let always = center(NotifyCondition::Always, true);
    assert!(!always.suppresses(FocusPosition::Foreground));
}

// @lat: [[test#GPUI Notification Gate#Pending focus is consumed once]]
#[test]
fn pending_focus_is_consumed_once_and_cleared_on_remove() {
    let mut center = center(NotifyCondition::Always, true);
    let session = SessionId::new();
    center.set_last_notified(session);
    assert_eq!(center.take_pending_focus(), Some(session));
    assert_eq!(center.take_pending_focus(), None);

    center.set_last_notified(session);
    center.remove(session);
    assert_eq!(center.take_pending_focus(), None);
}

// @lat: [[test#GPUI Notification Gate#State labels name the attention state]]
#[test]
fn state_labels_name_the_attention_state() {
    assert_eq!(state_label(&AiState::IdlePrompt), "Ready");
    assert_eq!(state_label(&AiState::WaitingForInput), "Waiting for input");
    assert_eq!(state_label(&AiState::PermissionPrompt), "Permission required");
    assert_eq!(state_label(&AiState::Error), "Attention");
}
