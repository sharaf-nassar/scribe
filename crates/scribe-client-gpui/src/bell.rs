//! Terminal bell routing: per-tab attention badge plus the system bell.
//!
//! Ports the winit client's `handle_bell_event`
//! ([`crate`](../../scribe-client/src/main.rs)) onto a GPUI entity. A
//! `ServerMessage::Bell` for a session raises a signal only when that session is
//! not the focused foreground pane (window unfocused, or the belling session is
//! a different or background tab) and no update is in progress — matching the
//! winit gate that requested window attention only in that case.
//!
//! When the gate passes, [`BellController`] records a per-session attention
//! badge (surfaced on the tab once the tab bar lands) and emits
//! [`BellEvent::Signal`] so the view rings the OS bell / requests window
//! attention. Focusing a tab clears its badge. A bell to the already-focused
//! foreground pane is suppressed, exactly like the winit client.

use std::collections::HashSet;

use gpui::{Context, EventEmitter};
use scribe_common::ids::SessionId;

/// Event emitted when a bell clears the suppression gate and must be signalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BellEvent {
    /// Ring the system bell / request window attention for `session_id`, which
    /// also just gained an attention badge.
    Signal { session_id: SessionId },
}

/// GPUI entity routing terminal bells to tab badges and the system bell.
///
/// Tracks the focus context (window focus, focused session, in-progress update)
/// needed to reproduce the winit suppression gate, plus the set of sessions
/// currently wearing an attention badge.
pub struct BellController {
    /// Whether the client window currently has OS focus.
    window_focused: bool,
    /// The session shown in the focused foreground pane, if any.
    focused_session: Option<SessionId>,
    /// Whether an update is downloading/installing (suppresses bell attention,
    /// matching the winit `update_available`/progress gate).
    update_in_progress: bool,
    /// Sessions currently wearing an attention badge.
    badged: HashSet<SessionId>,
}

impl EventEmitter<BellEvent> for BellController {}

impl Default for BellController {
    fn default() -> Self {
        Self::new()
    }
}

impl BellController {
    /// Create a controller with a focused window and no badges.
    #[must_use]
    pub fn new() -> Self {
        Self {
            window_focused: true,
            focused_session: None,
            update_in_progress: false,
            badged: HashSet::new(),
        }
    }

    /// Update whether the client window has OS focus.
    pub const fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// Update whether an update is in progress (suppresses bell attention).
    pub const fn set_update_in_progress(&mut self, in_progress: bool) {
        self.update_in_progress = in_progress;
    }

    /// Whether `session_id` currently wears an attention badge.
    #[must_use]
    pub fn has_badge(&self, session_id: SessionId) -> bool {
        self.badged.contains(&session_id)
    }

    /// Number of sessions wearing a badge.
    #[must_use]
    pub fn badge_count(&self) -> usize {
        self.badged.len()
    }

    /// Focus `session_id`'s tab: record it as the focused session and clear its
    /// attention badge. Emits no event; the badge simply retires.
    pub fn focus_session(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        self.focused_session = Some(session_id);
        if self.badged.remove(&session_id) {
            cx.notify();
        }
    }

    /// Handle a terminal bell for `session_id`.
    ///
    /// Signals (badge + [`BellEvent::Signal`]) only when the bell targets a
    /// session other than the focused foreground pane, or the window is
    /// unfocused, and no update is in progress. A bell to the focused
    /// foreground pane while focused and idle is suppressed.
    pub fn on_bell(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        let is_foreground = self.window_focused && self.focused_session == Some(session_id);
        if is_foreground || self.update_in_progress {
            return;
        }
        let newly_badged = self.badged.insert(session_id);
        cx.emit(BellEvent::Signal { session_id });
        if newly_badged {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use gpui::{AppContext as _, Entity, TestAppContext};
    use scribe_common::ids::SessionId;

    use super::{BellController, BellEvent};

    /// Collect every event a controller emits into a shared vector.
    fn record_events(
        controller: &Entity<BellController>,
        cx: &mut TestAppContext,
    ) -> Arc<Mutex<Vec<BellEvent>>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        cx.update(|app| {
            app.subscribe(controller, move |_, event: &BellEvent, _| {
                sink.lock().unwrap().push(*event);
            })
            .detach();
        });
        cx.update(|_| {});
        events
    }

    // @lat: [[client#GPUI Client Spike#Bell Routing]]
    #[gpui::test]
    fn bell_to_background_session_badges_and_signals(cx: &mut TestAppContext) {
        let foreground = SessionId::new();
        let background = SessionId::new();
        let controller = cx.new(|_| BellController::new());
        let events = record_events(&controller, cx);

        controller.update(cx, |c, cx| c.focus_session(foreground, cx));
        controller.update(cx, |c, cx| c.on_bell(background, cx));

        controller.read_with(cx, |c, _| {
            assert!(c.has_badge(background));
            assert!(!c.has_badge(foreground));
        });
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[BellEvent::Signal { session_id: background }]
        );
    }

    // @lat: [[client#GPUI Client Spike#Bell Routing]]
    #[gpui::test]
    fn bell_to_focused_foreground_pane_is_suppressed(cx: &mut TestAppContext) {
        let session = SessionId::new();
        let controller = cx.new(|_| BellController::new());
        let events = record_events(&controller, cx);

        controller.update(cx, |c, cx| c.focus_session(session, cx));
        controller.update(cx, |c, cx| c.on_bell(session, cx));

        controller.read_with(cx, |c, _| assert!(!c.has_badge(session)));
        assert!(events.lock().unwrap().is_empty(), "focused foreground bell is silent");
    }

    // @lat: [[client#GPUI Client Spike#Bell Routing]]
    #[gpui::test]
    fn bell_signals_focused_session_when_window_unfocused(cx: &mut TestAppContext) {
        let session = SessionId::new();
        let controller = cx.new(|_| BellController::new());
        let events = record_events(&controller, cx);

        controller.update(cx, |c, cx| c.focus_session(session, cx));
        controller.update(cx, |c, _| c.set_window_focused(false));
        controller.update(cx, |c, cx| c.on_bell(session, cx));

        controller.read_with(cx, |c, _| assert!(c.has_badge(session)));
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    // @lat: [[client#GPUI Client Spike#Bell Routing]]
    #[gpui::test]
    fn update_in_progress_suppresses_all_bells(cx: &mut TestAppContext) {
        let session = SessionId::new();
        let controller = cx.new(|_| BellController::new());
        let events = record_events(&controller, cx);

        controller.update(cx, |c, _| c.set_window_focused(false));
        controller.update(cx, |c, _| c.set_update_in_progress(true));
        controller.update(cx, |c, cx| c.on_bell(session, cx));

        controller.read_with(cx, |c, _| assert!(!c.has_badge(session)));
        assert!(events.lock().unwrap().is_empty());
    }

    // @lat: [[client#GPUI Client Spike#Bell Routing]]
    #[gpui::test]
    fn focusing_a_badged_tab_clears_its_badge(cx: &mut TestAppContext) {
        let session = SessionId::new();
        let controller = cx.new(|_| BellController::new());

        // Bell arrives while the tab is a background tab.
        controller.update(cx, |c, cx| c.on_bell(session, cx));
        controller.read_with(cx, |c, _| assert!(c.has_badge(session)));

        // Focusing the tab retires the badge.
        controller.update(cx, |c, cx| c.focus_session(session, cx));
        controller.read_with(cx, |c, _| {
            assert!(!c.has_badge(session));
            assert_eq!(c.badge_count(), 0);
        });
    }
}
