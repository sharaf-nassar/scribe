//! Cross-platform desktop notification dispatcher for the GPUI client.
//!
//! Exposes one API — [`spawn_dispatcher`], [`NotifReq`], and [`NotifOutput`] —
//! to the rest of the client. Platform divergence is hidden inside this module:
//! Linux uses raw `zbus` so every notification shares one D-Bus connection and
//! `replaces_id` keeps one toast per session; non-Linux platforms fall back to
//! a sink that drops every request (the macOS `notify-rust` path is deferred
//! with the rest of the macOS port, plan Phase H).
//!
//! The click-to-focus signal is decoupled from any concrete UI runtime: the
//! dispatcher emits [`NotifOutput::FocusSession`] on a caller-supplied channel
//! instead of a winit event-loop proxy, so the zbus transport and the
//! `replaces_id` coalescing are runtime-agnostic and unit-testable while the
//! GPUI event bridge lands in a later consumer bead. Ported from the legacy
//! client's `notification_dispatcher`.
//!
//! The `replaces_id` coalescing state machine ([`NotifState`]) and the
//! freedesktop `expire_timeout` mapping ([`expire_timeout_millis`]) are pure
//! and live here so they are covered by unit tests on every platform.

#[cfg(target_os = "linux")]
mod linux;

use std::collections::HashMap;

use scribe_common::config::NotifyTimeoutMode;
use scribe_common::ids::SessionId;
use tokio::sync::mpsc;

/// Signal emitted by the dispatcher back into the UI runtime. Currently the
/// only output is click-to-focus: the user clicked a notification (or its
/// default action) and the owning session's window/tab should be focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifOutput {
    /// The notification for `session_id` was activated; focus that session.
    FocusSession { session_id: SessionId },
}

/// Request to the dispatcher thread. Sent by the UI thread on notification
/// fire, session exit, and shutdown.
pub enum NotifReq {
    /// Show or replace the notification associated with `session_id`.
    Show(ShowReq),
    /// Close the notification for `session_id`, if any. Sent on session exit /
    /// `AiStateCleared` so stale toasts do not linger.
    Close { session_id: SessionId },
    /// Close every live notification and exit the dispatcher loop.
    Shutdown,
}

impl NotifReq {
    /// Build a close request for a session.
    #[must_use]
    pub fn close(session_id: SessionId) -> Self {
        Self::Close { session_id }
    }
}

/// Payload for [`NotifReq::Show`]. Bundled into a struct so the dispatcher's
/// show path stays under clippy's argument limit and new fields can land
/// without churning every call site.
pub struct ShowReq {
    pub session_id: SessionId,
    pub summary: String,
    pub body: String,
    /// The freedesktop spec exposes `expire_timeout`; mapped by
    /// [`expire_timeout_millis`].
    pub timeout_mode: NotifyTimeoutMode,
    /// Paired with [`NotifyTimeoutMode::Custom`].
    pub timeout_secs: u32,
}

impl ShowReq {
    /// Build a show request.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        summary: String,
        body: String,
        timeout_mode: NotifyTimeoutMode,
        timeout_secs: u32,
    ) -> Self {
        Self { session_id, summary, body, timeout_mode, timeout_secs }
    }
}

/// Map a [`NotifyTimeoutMode`] onto the freedesktop `expire_timeout` value:
/// `-1` lets the daemon pick its default, `0` keeps the toast until dismissed,
/// and `Custom` uses `timeout_secs` in milliseconds (saturating).
#[must_use]
pub fn expire_timeout_millis(mode: NotifyTimeoutMode, timeout_secs: u32) -> i32 {
    match mode {
        NotifyTimeoutMode::SystemDefault => -1,
        NotifyTimeoutMode::Custom => {
            i32::try_from(timeout_secs.saturating_mul(1000)).unwrap_or(i32::MAX)
        }
        NotifyTimeoutMode::Never => 0,
    }
}

/// `replaces_id` coalescing state: one toast per session.
///
/// Tracks the daemon-assigned notification id both ways so a state change for a
/// session reuses its live id via `replaces_id` (the freedesktop daemon swaps
/// the toast in place instead of stacking a new one), an incoming
/// `NotificationClosed` signal clears the mapping, and a session-exit close
/// finds the id to retire.
#[derive(Debug, Default)]
pub struct NotifState {
    by_id: HashMap<u32, SessionId>,
    by_session: HashMap<SessionId, u32>,
}

impl NotifState {
    /// Fresh empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `replaces_id` to pass to `notify` for `session_id` — the session's
    /// live notification id, or `0` when it has none (allocate a new toast).
    #[must_use]
    pub fn replaces_for(&self, session_id: SessionId) -> u32 {
        self.by_session.get(&session_id).copied().unwrap_or(0)
    }

    /// Record a shown notification. `replaces` is the id passed to `notify`;
    /// `new_id` is the id the daemon returned. When the daemon allocates a new
    /// id despite a non-zero `replaces` (the prior toast had already expired),
    /// the stale reverse mapping is dropped so it cannot mis-route a later
    /// click.
    pub fn record_shown(&mut self, session_id: SessionId, replaces: u32, new_id: u32) {
        if replaces != 0 && replaces != new_id {
            self.by_id.remove(&replaces);
        }
        self.by_id.insert(new_id, session_id);
        self.by_session.insert(session_id, new_id);
    }

    /// Remove and return the live notification id for `session_id`, if any
    /// (used to close the toast on session exit).
    pub fn take_session(&mut self, session_id: SessionId) -> Option<u32> {
        let id = self.by_session.remove(&session_id)?;
        self.by_id.remove(&id);
        Some(id)
    }

    /// The session that owns notification `id`, if it is still live (used to
    /// route a click-invoked action).
    #[must_use]
    pub fn session_for_id(&self, id: u32) -> Option<SessionId> {
        self.by_id.get(&id).copied()
    }

    /// Handle a daemon `NotificationClosed` signal: drop both mappings for `id`.
    pub fn on_closed(&mut self, id: u32) {
        if let Some(session_id) = self.by_id.remove(&id) {
            self.by_session.remove(&session_id);
        }
    }

    /// All live notification ids (used to close everything on shutdown).
    #[must_use]
    pub fn live_ids(&self) -> Vec<u32> {
        self.by_id.keys().copied().collect()
    }

    /// Drop all state after a shutdown close-all.
    pub fn clear(&mut self) {
        self.by_id.clear();
        self.by_session.clear();
    }
}

/// Spawn the platform-appropriate dispatcher on a dedicated thread and return
/// an unbounded sender. Click-to-focus signals are emitted on `out`. Dropping
/// the returned sender shuts the dispatcher down; sending [`NotifReq::Shutdown`]
/// also closes every live notification first.
///
/// Falls back to a sink that drops every request on non-Linux platforms so the
/// rest of the client compiles unchanged.
pub fn spawn_dispatcher(
    out: mpsc::UnboundedSender<NotifOutput>,
) -> mpsc::UnboundedSender<NotifReq> {
    #[cfg(target_os = "linux")]
    {
        linux::spawn(out)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Consume the sender rather than borrowing it: the Linux dispatcher
        // takes ownership, so the signature must stay by-value on every
        // platform, and closing it here is the honest no-op.
        drop(out);
        let (tx, _rx) = mpsc::unbounded_channel();
        tx
    }
}

#[cfg(test)]
mod tests;
