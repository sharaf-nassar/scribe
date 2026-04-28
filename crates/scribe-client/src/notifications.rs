//! Per-session AI state-transition tracking that decides when an OS
//! notification should fire (`Processing → IdlePrompt`,
//! `WaitingForInput`, or `PermissionPrompt`).
//!
//! Actual delivery is handled by [`crate::notification_dispatcher`] on
//! every platform — the tracker only owns the state machine that
//! turns AI state changes into [`NotificationPayload`] decisions and
//! the focus-on-activate fallback used by macOS click-to-focus.

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
    /// On macOS, the `Focused(true)` handler uses [`Self::take_pending_focus`]
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
