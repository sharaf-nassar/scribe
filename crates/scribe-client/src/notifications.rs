//! Per-session AI state-transition tracking that decides when a desktop
//! notification should fire, and the click-to-focus signal it routes back.
//!
//! Ported from the winit client's `notifications.rs`. Delivery is
//! [`crate::notification_dispatcher`]'s job; this module owns only the state
//! machine that turns AI state changes into [`NotificationPayload`] decisions,
//! the focus-condition gate the winit client kept inline in
//! `maybe_fire_notification`, and the focus-on-activate fallback used when the
//! platform activates the app without telling us which toast was clicked.
//!
//! The GPUI split mirrors the terminal bell's: the IPC reader cannot judge
//! focus, build a summary, or touch the dispatcher, so it only queues an
//! [`AiNotice`] per AI transition and the foreground drains the queue on its
//! lifecycle tick. Everything here except [`NotificationCenter::request_focus`]
//! is pure, so the gate is unit-tested headlessly.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use gpui::{Context, EventEmitter};
use scribe_common::{
    ai_state::AiState,
    config::{NotificationsConfig, NotifyCondition},
    ids::SessionId,
};

/// How long after showing a notification the focus-on-activate fallback stays
/// valid. A click that lands later is treated as the user navigating back on
/// their own, so the tab is not yanked out from under them.
pub const NOTIFICATION_FOCUS_WINDOW: Duration = Duration::from_secs(30);

/// One AI transition the IPC reader took off the wire for the foreground.
///
/// The reader owns no config, no focus, and no dispatcher handle, so it records
/// the transition verbatim and lets the foreground decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiNotice {
    /// `session_id` moved to `state` (`ServerMessage::AiStateChanged`).
    StateChanged {
        /// The session whose AI state changed.
        session_id: SessionId,
        /// The state it moved to.
        state: AiState,
    },
    /// `session_id` has no AI state any more — cleared by the server, or the
    /// session exited. Any live toast for it must be retired.
    Cleared {
        /// The session whose notification state is retired.
        session_id: SessionId,
    },
}

/// A transition that warrants a desktop notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationPayload {
    /// The session that wants attention.
    pub session_id: SessionId,
    /// The attention state it reached.
    pub state: AiState,
}

/// Signal emitted when a clicked notification asks for its session's tab.
///
/// Emitted as a GPUI event rather than handled inline because raising the
/// window is a `Window` call, and only a `subscribe_in` handler holds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationEvent {
    /// Select `session_id`'s tab and raise the window.
    FocusSession {
        /// The session whose toast was activated.
        session_id: SessionId,
    },
}

/// Where the notifying session sits relative to the user's attention, which is
/// the only input the [`NotifyCondition`] gate reads besides the config itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPosition {
    /// The window has focus and the session is the pane inside it.
    Foreground,
    /// The window has focus but the session is a background tab.
    BackgroundTab,
    /// The window itself does not have focus, so every pane is background.
    WindowUnfocused,
}

impl FocusPosition {
    /// Place `session_id` against the window's focus and the session currently
    /// in the focused pane.
    ///
    /// An unfocused window collapses both tab cases: with the keyboard
    /// elsewhere, nothing this window shows is in front of the user.
    #[must_use]
    pub fn resolve(
        window_focused: bool,
        focused_session: Option<SessionId>,
        session_id: SessionId,
    ) -> Self {
        if !window_focused {
            Self::WindowUnfocused
        } else if focused_session == Some(session_id) {
            Self::Foreground
        } else {
            Self::BackgroundTab
        }
    }
}

/// The short state label a notification summary is built around.
///
/// Only the three attention states can reach a fired notification; the catch-all
/// exists so a future `AiState` variant degrades to a generic label instead of
/// failing to build.
#[must_use]
pub const fn state_label(state: &AiState) -> &'static str {
    match state {
        AiState::IdlePrompt => "Ready",
        AiState::WaitingForInput => "Waiting for input",
        AiState::PermissionPrompt => "Permission required",
        _ => "Attention",
    }
}

/// Tracks previous AI state per session to detect `Processing → attention`
/// transitions, and decides whether a desktop notification should fire.
pub struct NotificationCenter {
    previous_states: HashMap<SessionId, AiState>,
    config: NotificationsConfig,
    /// Session of the most recently fired notification, used by the
    /// focus-on-activate fallback.
    last_notified: Option<(SessionId, Instant)>,
}

impl EventEmitter<NotificationEvent> for NotificationCenter {}

impl NotificationCenter {
    /// A tracker seeded with the active `[notifications]` config.
    #[must_use]
    pub fn new(config: NotificationsConfig) -> Self {
        Self { previous_states: HashMap::new(), config, last_notified: None }
    }

    /// Adopt a config reloaded from disk.
    pub fn reconfigure(&mut self, config: NotificationsConfig) {
        self.config = config;
    }

    /// The active config (the dispatcher needs its timeout fields).
    #[must_use]
    pub const fn config(&self) -> &NotificationsConfig {
        &self.config
    }

    /// Fold one AI state change in, returning the payload when it warrants a
    /// notification.
    ///
    /// The transition that fires is `Processing → attention`: an attention
    /// state the session was already sitting in does not re-notify, and a
    /// session that never processed anything (a replayed `SessionList`) does
    /// not notify on its first observed state either.
    pub fn on_ai_state_changed(
        &mut self,
        session_id: SessionId,
        new_state: &AiState,
    ) -> Option<NotificationPayload> {
        let was_processing = self
            .previous_states
            .get(&session_id)
            .is_some_and(|previous| *previous == AiState::Processing);
        self.previous_states.insert(session_id, new_state.clone());

        if !self.config.enabled {
            return None;
        }
        let is_attention = matches!(
            new_state,
            AiState::IdlePrompt | AiState::WaitingForInput | AiState::PermissionPrompt
        );
        (was_processing && is_attention)
            .then(|| NotificationPayload { session_id, state: new_state.clone() })
    }

    /// Whether the configured [`NotifyCondition`] suppresses this notification.
    ///
    /// The three conditions are the winit client's `maybe_fire_notification`
    /// gate, restated over [`FocusPosition`] so each arm reads as the situation
    /// it describes rather than as a pair of booleans.
    #[must_use]
    pub const fn suppresses(&self, position: FocusPosition) -> bool {
        match self.config.condition {
            NotifyCondition::WhenUnfocused => !matches!(position, FocusPosition::WindowUnfocused),
            NotifyCondition::WhenUnfocusedOrBackgroundTab => {
                matches!(position, FocusPosition::Foreground)
            }
            NotifyCondition::Always => false,
        }
    }

    /// Record that a toast was just shown for `session_id`.
    pub fn set_last_notified(&mut self, session_id: SessionId) {
        self.last_notified = Some((session_id, Instant::now()));
    }

    /// Consume the recent notification's session so an activation that carries
    /// no notification id can still switch to the right tab.
    ///
    /// Returns `None` once [`NOTIFICATION_FOCUS_WINDOW`] has elapsed.
    pub fn take_pending_focus(&mut self) -> Option<SessionId> {
        let (session_id, when) = self.last_notified.take()?;
        (when.elapsed() < NOTIFICATION_FOCUS_WINDOW).then_some(session_id)
    }

    /// Drop everything known about a session (exit, or `AiStateCleared`).
    pub fn remove(&mut self, session_id: SessionId) {
        self.previous_states.remove(&session_id);
        if self.last_notified.as_ref().is_some_and(|(id, _)| *id == session_id) {
            self.last_notified = None;
        }
    }

    /// Ask the view to focus `session_id` — the dispatcher reported a click on
    /// that session's toast.
    ///
    /// Consumes the focus-on-activate fallback in the same breath so the
    /// activation this raise causes does not dispatch the same switch twice.
    pub fn request_focus(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        self.last_notified = None;
        cx.emit(NotificationEvent::FocusSession { session_id });
    }
}

#[cfg(test)]
mod tests;
