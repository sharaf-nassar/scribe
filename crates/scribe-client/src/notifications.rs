//! Desktop notification support for AI session state changes.
//!
//! Tracks per-session AI state transitions and fires OS notifications
//! when a session leaves `Processing` and enters an attention state
//! (`IdlePrompt`, `WaitingForInput`, `PermissionPrompt`).
//!
//! On Linux, clicking a notification dispatches `FocusSession` via the
//! D-Bus `wait_for_action` callback.  On macOS, `notify-rust` does not
//! support click callbacks, so the tracker records the last-notified
//! session and the `Focused(true)` handler calls `take_pending_focus`
//! to switch tabs when the OS activates the window after a click.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use scribe_common::ai_state::{AiProcessState, AiState};
use scribe_common::config::NotificationsConfig;
use scribe_common::ids::SessionId;

/// How long after showing a notification the focus-on-activate fallback
/// remains valid.  If the user clicks the notification after this window
/// the tab switch is skipped (they likely navigated away intentionally).
const NOTIFICATION_FOCUS_WINDOW: Duration = Duration::from_secs(30);
/// Linux bell events can arrive immediately after the richer AI desktop
/// notification, and GNOME renders the urgency hint as a second shell-level
/// "<app> is ready" toast. Keep the suppression narrow so later bells still
/// raise attention normally.
const LINUX_BELL_SUPPRESSION_WINDOW: Duration = Duration::from_secs(2);

/// Payload produced when a notification should be shown.
pub struct NotificationPayload {
    pub session_id: SessionId,
    pub state: AiState,
}

/// Tracks previous AI state per session to detect `Processing → attention`
/// transitions and decide whether a desktop notification should fire.
pub struct NotificationTracker {
    previous_states: HashMap<SessionId, AiState>,
    config: NotificationsConfig,
    /// Session from the most recently fired notification, used on macOS
    /// to switch tabs when the window gains focus after a notification click.
    last_notified: Option<(SessionId, Instant)>,
}

impl NotificationTracker {
    #[must_use]
    pub fn new(config: NotificationsConfig) -> Self {
        Self { previous_states: HashMap::new(), config, last_notified: None }
    }

    /// Update config after a live reload.
    pub fn reconfigure(&mut self, config: NotificationsConfig) {
        self.config = config;
    }

    /// Check whether a notification should fire for this AI state change.
    ///
    /// Returns `Some(payload)` when the session transitioned from
    /// `Processing` to an attention state and notifications are enabled.
    /// The caller is responsible for focus-based suppression.
    pub fn on_ai_state_changed(
        &mut self,
        session_id: SessionId,
        new_state: &AiProcessState,
    ) -> Option<NotificationPayload> {
        let was_processing =
            self.previous_states.get(&session_id).is_some_and(|prev| *prev == AiState::Processing);

        self.previous_states.insert(session_id, new_state.state.clone());

        if !self.config.enabled {
            return None;
        }

        let is_attention = matches!(
            new_state.state,
            AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
        );

        if was_processing && is_attention {
            Some(NotificationPayload { session_id, state: new_state.state.clone() })
        } else {
            None
        }
    }

    /// Record that a notification was just shown for this session.
    ///
    /// On macOS, the `Focused(true)` handler uses [`take_pending_focus`]
    /// to consume this and dispatch `handle_focus_session`.
    pub fn set_last_notified(&mut self, session_id: SessionId) {
        self.last_notified = Some((session_id, Instant::now()));
    }

    /// Return whether Linux should suppress a bell-driven urgency hint
    /// because the same session just fired an explicit AI notification.
    #[must_use]
    pub fn should_suppress_linux_bell_attention(&self, session_id: SessionId) -> bool {
        self.last_notified.as_ref().is_some_and(|(id, when)| {
            *id == session_id && when.elapsed() < LINUX_BELL_SUPPRESSION_WINDOW
        })
    }

    /// If a notification was shown recently, consume and return the session
    /// ID so the caller can switch to it.  Returns `None` if no notification
    /// is pending or the notification has expired.
    pub fn take_pending_focus(&mut self) -> Option<SessionId> {
        let (session_id, when) = self.last_notified.take()?;
        if when.elapsed() < NOTIFICATION_FOCUS_WINDOW { Some(session_id) } else { None }
    }

    /// Remove all state for a session (on exit or clear).
    pub fn remove(&mut self, session_id: SessionId) {
        self.previous_states.remove(&session_id);
        if self.last_notified.as_ref().is_some_and(|(id, _)| *id == session_id) {
            self.last_notified = None;
        }
    }

    /// Current config reference.
    #[must_use]
    pub fn config(&self) -> &NotificationsConfig {
        &self.config
    }
}

/// Send a desktop notification and optionally wait for the user to click it.
///
/// On Linux this function **blocks** on D-Bus `wait_for_action` and must be
/// called from a spawned thread.  On macOS the call returns immediately —
/// click handling is done via the focus-on-activate fallback in the caller.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn fire_os_notification(
    summary: &str,
    body: &str,
    session_id: SessionId,
    proxy: &winit::event_loop::EventLoopProxy<super::ipc_client::UiEvent>,
    config: &NotificationsConfig,
) {
    use notify_rust::Notification;

    let identity = scribe_common::app::current_identity();
    let mut notif = Notification::new();
    notif.summary(summary).body(body).appname(identity.window_title_name());

    #[cfg(target_os = "linux")]
    {
        use notify_rust::Timeout;
        use scribe_common::config::NotifyTimeoutMode;

        notif.icon(identity.slug());
        notif.hint(notify_rust::Hint::DesktopEntry(identity.slug().to_owned()));
        notif.action("default", "Focus");
        match config.timeout_mode {
            NotifyTimeoutMode::SystemDefault => {
                notif.timeout(Timeout::Default);
            }
            NotifyTimeoutMode::Custom => {
                let millis = config.timeout_secs.saturating_mul(1000);
                notif.timeout(Timeout::Milliseconds(millis));
            }
            NotifyTimeoutMode::Never => {
                notif.timeout(Timeout::Never);
            }
        }
    }

    match notif.show() {
        Ok(handle) => {
            #[cfg(target_os = "linux")]
            fire_on_click(handle, proxy, session_id);
            // macOS: notify-rust does not support wait_for_action on
            // NSUserNotification.  Click-to-focus is handled by the
            // focus-on-activate fallback — macOS brings the app to the
            // foreground when the user clicks the notification, and the
            // Focused(true) handler calls take_pending_focus().
            #[cfg(target_os = "macos")]
            {
                let _ = (config, handle, proxy);
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "desktop notification failed");
        }
    }
}

/// Block on a Linux D-Bus notification action callback and dispatch
/// `FocusSession` when the user clicks the notification body.
#[cfg(target_os = "linux")]
fn fire_on_click(
    handle: notify_rust::NotificationHandle,
    proxy: &winit::event_loop::EventLoopProxy<super::ipc_client::UiEvent>,
    session_id: SessionId,
) {
    use scribe_common::protocol::AutomationAction;

    use super::ipc_client::UiEvent;

    let proxy = proxy.clone();
    handle.wait_for_action(|action| {
        if action == "default" {
            drop(proxy.send_event(UiEvent::RunAction {
                action: AutomationAction::FocusSession { session_id },
            }));
        }
    });
}
