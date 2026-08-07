//! Window close / quit / focus lifecycle for the GPUI client.
//!
//! Four client messages (`CloseWindow`, `QuitAll`, `ListWindows`,
//! `FocusChanged`) and three server messages (`WindowClosed`, `WindowList`,
//! `QuitRequested`) form one request/acknowledge conversation that spans both
//! of the client's threads: the GPUI view raises a close, a quit, or a focus
//! change from a real UI event, and the IPC reader receives the server's
//! answer. [`WindowLifecycle`] is the single piece of state they share behind a
//! mutex.
//!
//! It holds the window id the server assigned in `Welcome` (the id
//! `CloseWindow` has to name), the shutdown the client is waiting to have
//! acknowledged, the acknowledged exit the foreground drains, the controller
//! list projected out of the latest `WindowList` reply, and the session focus
//! last reported to the server so a repeat is never re-sent.
//!
//! Ported from the winit client's `handle_quit_all` / `handle_close_window` /
//! `handle_window_closed` / `handle_quit_requested` / `notify_focus_change`
//! group, minus the event-loop plumbing: every decision here is pure, so it is
//! tested without a window, and the caller performs the two effects — sending
//! on the IPC sink and quitting the app.

use scribe_common::{
    ids::{SessionId, WindowId},
    protocol::{ControllerInfo, WindowInfo},
};

/// A shutdown the client has asked the server for and is waiting to have
/// acknowledged.
///
/// Only one can be in flight: the winit client guards both entry points with
/// `if self.pending_shutdown.is_some() { return }` so a second Enter on the
/// close dialog cannot send a second frame, and so does this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingShutdown {
    /// `QuitAll` was sent; the server answers every window with `QuitRequested`.
    QuitAll,
    /// `CloseWindow` was sent for this window; the server answers `WindowClosed`.
    CloseWindow {
        /// The window the client asked the server to destroy.
        window_id: WindowId,
    },
}

/// Why the shell must tear this window down.
///
/// Produced only by a server acknowledgement, never by the local request, so
/// the window stays usable if the server never answers — exactly the winit
/// behaviour, where the event loop exits from the ack handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The server broadcast `QuitRequested` (because this window or another one
    /// asked for a quit-all). Sessions are preserved and can be reattached.
    QuitRequested,
    /// The server confirmed this window's `CloseWindow`; its sessions are gone.
    WindowClosed,
}

/// One focus transition to put on the wire as `ClientMessage::FocusChanged`.
///
/// The server relays these to PTY applications that enabled DECSET 1004, so the
/// pair is directional: `lost` is the session the server currently believes is
/// focused and `gained` is the one replacing it. Either side may be `None` —
/// window blur reports a loss with no gain, and the first focus of a session
/// reports a gain with no loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusReport {
    /// The session that gained focus, or `None` when the window lost OS focus.
    pub gained: Option<SessionId>,
    /// The session that lost focus, or `None` when nothing was focused before.
    pub lost: Option<SessionId>,
}

/// Shared window-lifecycle state: the window's identity, its in-flight
/// shutdown, the acknowledged exit, the remote-controller summary, whether the
/// OS window is active, and the focus last reported to the server.
#[derive(Debug)]
pub struct WindowLifecycle {
    window_id: Option<WindowId>,
    pending: Option<PendingShutdown>,
    exit: Option<ExitReason>,
    controllers: Vec<ControllerInfo>,
    window_active: bool,
    focus: Option<SessionId>,
    siblings: Vec<WindowId>,
}

impl Default for WindowLifecycle {
    /// A freshly opened window is the one the user just asked for, so it starts
    /// active; the shell's activation observer corrects this on the first real
    /// activation change.
    fn default() -> Self {
        Self {
            window_id: None,
            pending: None,
            exit: None,
            controllers: Vec::new(),
            window_active: true,
            focus: None,
            siblings: Vec::new(),
        }
    }
}

impl WindowLifecycle {
    /// A lifecycle for a window that has not handshaken yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the window id the server handed back in `Welcome`.
    ///
    /// Until this arrives the client cannot name itself, so "Kill Window" has
    /// nothing to send and [`Self::begin_close_window`] refuses.
    pub fn adopt_window(&mut self, window_id: WindowId) {
        self.window_id = Some(window_id);
    }

    /// The server-assigned id of this window, once `Welcome` has arrived.
    #[must_use]
    pub const fn window_id(&self) -> Option<WindowId> {
        self.window_id
    }

    /// Park the other windows `Welcome` reported as still having sessions but no
    /// client.
    ///
    /// These are the user's other windows from before the client exited: the
    /// server keeps a window's sessions when the client goes away, so a restart
    /// adopts one of them and is handed the rest to reopen. Parked rather than
    /// acted on because the reader thread cannot open a window; the foreground
    /// drains this with [`Self::take_sibling_windows`]. Ids already parked are
    /// not re-added, so a repeated report cannot queue a window twice.
    pub fn park_sibling_windows(&mut self, windows: Vec<WindowId>) {
        for window_id in windows {
            if !self.siblings.contains(&window_id) {
                self.siblings.push(window_id);
            }
        }
    }

    /// Take the parked sibling windows, leaving none behind.
    pub fn take_sibling_windows(&mut self) -> Vec<WindowId> {
        std::mem::take(&mut self.siblings)
    }

    /// The shutdown currently awaiting a server acknowledgement.
    #[must_use]
    pub const fn pending(&self) -> Option<PendingShutdown> {
        self.pending
    }

    /// Claim the quit-all slot. Returns `false` when a shutdown is already in
    /// flight, in which case no `QuitAll` may be sent.
    pub fn begin_quit_all(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.pending = Some(PendingShutdown::QuitAll);
        true
    }

    /// Claim the close-window slot, yielding the id `CloseWindow` must carry.
    ///
    /// `None` when a shutdown is already in flight or the server has not yet
    /// told this connection which window it is.
    pub fn begin_close_window(&mut self) -> Option<WindowId> {
        if self.pending.is_some() {
            return None;
        }
        let window_id = self.window_id?;
        self.pending = Some(PendingShutdown::CloseWindow { window_id });
        Some(window_id)
    }

    /// Release the shutdown slot for a request that never reached the wire, so
    /// a dropped writer does not wedge the window in a shutdown it cannot
    /// complete.
    pub fn abandon_shutdown(&mut self) {
        self.pending = None;
    }

    /// Apply `ServerMessage::QuitRequested`: every window saves and exits,
    /// whether or not this one asked for the quit.
    pub fn on_quit_requested(&mut self) {
        self.pending = None;
        self.exit = Some(ExitReason::QuitRequested);
    }

    /// Apply `ServerMessage::WindowClosed`, returning whether it acknowledged
    /// *this* window's pending close.
    ///
    /// An ack for a window the client never asked about is ignored rather than
    /// obeyed, mirroring the winit client's "ignoring unexpected `WindowClosed`
    /// ack" branch: an unrelated ack must never close a live window.
    pub fn on_window_closed(&mut self, window_id: WindowId) -> bool {
        if self.pending != Some(PendingShutdown::CloseWindow { window_id }) {
            return false;
        }
        self.pending = None;
        self.exit = Some(ExitReason::WindowClosed);
        true
    }

    /// Take the acknowledged exit, if one is due. The foreground drains this on
    /// its lifecycle tick and quits the app.
    pub fn take_exit(&mut self) -> Option<ExitReason> {
        self.exit.take()
    }

    /// Fold a `WindowList` reply into the remote-controller summary the status
    /// bar renders, returning whether the summary actually changed.
    ///
    /// Only windows a remote peer currently controls carry a `controller`, so
    /// the projection is the same `filter_map` the winit client's
    /// `handle_local_window_list` performs, and an unchanged list skips the
    /// repaint.
    pub fn set_windows(&mut self, windows: Vec<WindowInfo>) -> bool {
        let controllers: Vec<ControllerInfo> =
            windows.into_iter().filter_map(|window| window.controller).collect();
        if controllers == self.controllers {
            return false;
        }
        self.controllers = controllers;
        true
    }

    /// Remote peers controlling windows on this machine, newest reply wins.
    #[must_use]
    pub fn controllers(&self) -> &[ControllerInfo] {
        &self.controllers
    }

    /// Record whether the OS window currently holds focus.
    ///
    /// Kept here rather than on the view so the two axes of a focus report —
    /// window activation and which pane is attached — are compared in one
    /// place, and so a tab switch made while the window is blurred stays
    /// silent.
    pub fn set_window_active(&mut self, active: bool) {
        self.window_active = active;
    }

    /// Whether the OS window is currently active, as last observed.
    #[must_use]
    pub const fn window_active(&self) -> bool {
        self.window_active
    }

    /// Resolve the focus transition to report for the session the window is
    /// currently showing.
    ///
    /// The two axes collapse into one value — an inactive window focuses
    /// nothing — so a window blur, a window focus, and a tab switch all become
    /// the same comparison against the last reported value. `None` means the
    /// server already believes what is true, which is what keeps a poll from
    /// re-sending the same report every tick.
    pub fn focus_change(&mut self, session: Option<SessionId>) -> Option<FocusReport> {
        let focused = if self.window_active { session } else { None };
        if focused == self.focus {
            return None;
        }
        let report = FocusReport { gained: focused, lost: self.focus };
        self.focus = focused;
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(controller: Option<&str>) -> WindowInfo {
        WindowInfo {
            window_id: WindowId::new(),
            session_count: 1,
            connected: true,
            workspace_names: Vec::new(),
            controller: controller.map(|device_name| ControllerInfo {
                device_name: device_name.to_owned(),
                login_name: "alex".to_owned(),
            }),
            participants: Vec::new(),
            mode: None,
            participant_count: 0,
        }
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Window Lifecycle#Close and quit wait for their acknowledgement]]
    #[test]
    fn close_and_quit_wait_for_their_acknowledgement() {
        let mut lifecycle = WindowLifecycle::new();
        // "Kill Window" is inert until `Welcome` names this window.
        assert_eq!(lifecycle.begin_close_window(), None);
        assert_eq!(lifecycle.pending(), None);

        let window_id = WindowId::new();
        lifecycle.adopt_window(window_id);
        assert_eq!(lifecycle.window_id(), Some(window_id));
        assert_eq!(lifecycle.begin_close_window(), Some(window_id));
        assert_eq!(lifecycle.pending(), Some(PendingShutdown::CloseWindow { window_id }));
        // A second confirmation must not put a second frame on the wire.
        assert_eq!(lifecycle.begin_close_window(), None);
        assert!(!lifecycle.begin_quit_all());
        // Requesting is not exiting: only the server's ack ends the window.
        assert_eq!(lifecycle.take_exit(), None);

        assert!(lifecycle.on_window_closed(window_id));
        assert_eq!(lifecycle.pending(), None);
        assert_eq!(lifecycle.take_exit(), Some(ExitReason::WindowClosed));
        assert_eq!(lifecycle.take_exit(), None);

        let mut quitting = WindowLifecycle::new();
        assert!(quitting.begin_quit_all());
        assert_eq!(quitting.pending(), Some(PendingShutdown::QuitAll));
        quitting.on_quit_requested();
        assert_eq!(quitting.take_exit(), Some(ExitReason::QuitRequested));

        // A quit broadcast caused by another window still exits this one.
        let mut bystander = WindowLifecycle::new();
        bystander.on_quit_requested();
        assert_eq!(bystander.take_exit(), Some(ExitReason::QuitRequested));
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Window Lifecycle#An unrelated close ack is ignored]]
    #[test]
    fn an_unrelated_close_ack_is_ignored() {
        let mut lifecycle = WindowLifecycle::new();
        lifecycle.adopt_window(WindowId::new());

        // No close is pending at all.
        assert!(!lifecycle.on_window_closed(WindowId::new()));
        assert_eq!(lifecycle.take_exit(), None);

        let mine = lifecycle.begin_close_window().unwrap();
        // An ack naming somebody else's window leaves ours pending.
        assert!(!lifecycle.on_window_closed(WindowId::new()));
        assert_eq!(lifecycle.take_exit(), None);
        assert_eq!(lifecycle.pending(), Some(PendingShutdown::CloseWindow { window_id: mine }));
        assert!(lifecycle.on_window_closed(mine));
        assert_eq!(lifecycle.take_exit(), Some(ExitReason::WindowClosed));
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Window Lifecycle#Focus reports collapse window and session state]]
    #[test]
    fn focus_reports_collapse_window_and_session_state() {
        let mut lifecycle = WindowLifecycle::new();
        let first = SessionId::new();
        let second = SessionId::new();

        // A newly opened window starts active, with no pane to focus, so
        // nothing is sent.
        assert!(lifecycle.window_active());
        assert_eq!(lifecycle.focus_change(None), None);
        assert_eq!(
            lifecycle.focus_change(Some(first)),
            Some(FocusReport { gained: Some(first), lost: None })
        );
        // Steady state re-reports nothing, which is what makes the poll cheap.
        assert_eq!(lifecycle.focus_change(Some(first)), None);
        // A tab switch is a gain and a loss in one report.
        assert_eq!(
            lifecycle.focus_change(Some(second)),
            Some(FocusReport { gained: Some(second), lost: Some(first) })
        );
        // Window blur loses the pane even though the tab did not change.
        lifecycle.set_window_active(false);
        assert_eq!(
            lifecycle.focus_change(Some(second)),
            Some(FocusReport { gained: None, lost: Some(second) })
        );
        assert_eq!(lifecycle.focus_change(Some(second)), None);
        // A tab switch while blurred stays silent, then re-focus reports the
        // session that is actually on screen.
        assert_eq!(lifecycle.focus_change(Some(first)), None);
        lifecycle.set_window_active(true);
        assert_eq!(
            lifecycle.focus_change(Some(first)),
            Some(FocusReport { gained: Some(first), lost: None })
        );
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Window Lifecycle#Window list projects remote controllers]]
    #[test]
    fn window_list_projects_remote_controllers() {
        let mut lifecycle = WindowLifecycle::new();
        assert!(lifecycle.controllers().is_empty());

        // Locally-controlled windows carry no controller and contribute nothing.
        assert!(!lifecycle.set_windows(vec![window(None), window(None)]));
        assert!(lifecycle.controllers().is_empty());

        assert!(lifecycle.set_windows(vec![window(Some("laptop")), window(None)]));
        assert_eq!(lifecycle.controllers().len(), 1);
        assert_eq!(
            lifecycle.controllers().first().map(|info| info.device_name.as_str()),
            Some("laptop")
        );
        // An identical reply is not a repaint.
        assert!(!lifecycle.set_windows(vec![window(Some("laptop")), window(None)]));
        assert!(lifecycle.set_windows(vec![window(None)]));
        assert!(lifecycle.controllers().is_empty());
    }

    // @lat: [[test#GPUI Client Headless Suites#GPUI Window Lifecycle#Sibling windows are parked once and drained once]]
    #[test]
    fn sibling_windows_are_parked_once_and_drained_once() {
        let mut lifecycle = WindowLifecycle::new();
        assert!(lifecycle.take_sibling_windows().is_empty());

        let (first, second) = (WindowId::new(), WindowId::new());
        lifecycle.park_sibling_windows(vec![first, second]);
        // A redial re-reports the windows still waiting for a client; the ones
        // already queued must not be queued a second time.
        lifecycle.park_sibling_windows(vec![second]);
        assert_eq!(lifecycle.take_sibling_windows(), vec![first, second]);

        // Draining empties the queue, so a later tick cannot reopen them.
        assert!(lifecycle.take_sibling_windows().is_empty());
    }
}
