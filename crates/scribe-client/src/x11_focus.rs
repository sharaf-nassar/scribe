//! X11 active-window guard for the GPUI client.
//!
//! Queries `_NET_ACTIVE_WINDOW` on the root window to detect when a compositor
//! overlay (e.g. GNOME Screenshot) obscures this window without sending an X11
//! focus event, and debounces key events for a short period after re-activation
//! to catch stray keystrokes (e.g. Enter to confirm a screenshot) that arrive
//! just after the overlay closes.
//!
//! Ported from the legacy winit client's `x11_focus.rs`. The X11 window id is
//! now extracted from GPUI's `RawWindowHandle` (Xcb/Xlib) via
//! [`xcb_window_id`] rather than winit; the direct `_NET_ACTIVE_WINDOW`
//! comparison is preserved (per the XID capability spike, `scribe-38e.13`).
//! Non-X11 backends (Wayland, headless) yield no XID and the guard is not
//! enabled. The reactivation state machine is factored into the pure
//! [`ReactivationDebounce`] so its semantics are unit-tested without a display
//! server; the visual E2E exercises the live `_NET_ACTIVE_WINDOW` path.

use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "linux")]
use x11rb::connection::Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
#[cfg(target_os = "linux")]
use x11rb::rust_connection::RustConnection;

/// Debounce window after the compositor overlay dismisses.
pub const REACTIVATION_DEBOUNCE: Duration = Duration::from_millis(300);

/// Extract the X11 window id from a GPUI window (or any `HasWindowHandle`).
///
/// Returns `None` on Wayland, macOS, or any backend that does not expose an
/// Xlib/Xcb handle, which is the signal that the focus guard should stay off.
#[must_use]
#[cfg(target_os = "linux")]
pub fn xcb_window_id(handle_source: &impl HasWindowHandle) -> Option<u32> {
    let handle = handle_source.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

/// Non-Linux platforms never expose an X11 window id.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub fn xcb_window_id<T>(_handle_source: &T) -> Option<u32> {
    None
}

/// Pure reactivation debounce state machine, shared by [`X11FocusGuard`].
///
/// Tracks whether the window has been inactive and, on the inactive→active
/// transition, starts a debounce so stray keystrokes arriving just after a
/// compositor overlay closes are still suppressed. Kept free of any X11
/// dependency so its timing semantics are directly testable.
#[derive(Debug, Default)]
pub struct ReactivationDebounce {
    /// Whether the window has been inactive since the last re-activation.
    was_inactive: bool,
    /// When the inactive→active transition was first observed. The debounce is
    /// measured from this instant.
    reactivated_at: Option<Instant>,
}

impl ReactivationDebounce {
    /// Update state after observing whether our window is active, returning
    /// `true` when keyboard input should be suppressed.
    ///
    /// `now` is the observation instant; suppression holds while inactive and
    /// for [`REACTIVATION_DEBOUNCE`] after the inactive→active transition.
    pub fn observe(&mut self, is_active: bool, now: Instant) -> bool {
        if !is_active {
            self.was_inactive = true;
            return true;
        }

        // Transition: inactive → active. Start the debounce from this instant so
        // the stray keystroke that triggered this observation is within it.
        if self.was_inactive {
            self.was_inactive = false;
            self.reactivated_at = Some(now);
        }

        if let Some(started) = self.reactivated_at {
            if now.duration_since(started) < REACTIVATION_DEBOUNCE {
                return true;
            }
            self.reactivated_at = None;
        }

        false
    }

    /// Record an inactive observation without evaluating the debounce (used by
    /// the periodic `poll`, which never suppresses on its own).
    pub fn note_inactive(&mut self) {
        self.was_inactive = true;
    }

    /// Note an inactive→active transition observed by the periodic poll,
    /// starting the debounce from `now` if the window had been inactive.
    pub fn note_active(&mut self, now: Instant) {
        if self.was_inactive {
            self.was_inactive = false;
            self.reactivated_at = Some(now);
        }
    }

    /// Clear the debounce on a genuine focus event (compositor overlays do not
    /// send focus events, so this only fires for real focus transitions).
    pub fn clear(&mut self) {
        self.was_inactive = false;
        self.reactivated_at = None;
    }
}

/// Polls `_NET_ACTIVE_WINDOW` and suppresses keyboard input while our window is
/// not the active one (or was not active very recently).
pub struct X11FocusGuard {
    #[cfg(target_os = "linux")]
    conn: RustConnection,
    #[cfg(target_os = "linux")]
    root: u32,
    #[cfg(target_os = "linux")]
    net_active_window: u32,
    #[cfg(target_os = "linux")]
    our_window: u32,
    #[cfg(target_os = "linux")]
    debounce: ReactivationDebounce,
    /// Off Linux there is no constructor — [`X11FocusGuard::from_window_handle`]
    /// always yields `None` — so the guard is uninhabited rather than merely
    /// empty. Stating that in the type keeps the platform-neutral methods below
    /// honest: they discharge `self` by matching a value that cannot exist,
    /// instead of quietly ignoring it.
    #[cfg(not(target_os = "linux"))]
    never: std::convert::Infallible,
}

impl X11FocusGuard {
    /// Attempt to open an independent X11 connection and prepare the guard.
    ///
    /// Returns `None` when X11 is unavailable (e.g. pure Wayland) or when the
    /// connection/atom intern fails for any reason.
    #[must_use]
    #[cfg(target_os = "linux")]
    pub fn new(our_x11_window_id: u32) -> Option<Self> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let setup = conn.setup();
        let screen = setup.roots.get(screen_num)?;
        let root = screen.root;

        let atom_reply = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?;

        Some(Self {
            conn,
            root,
            net_active_window: atom_reply.atom,
            our_window: our_x11_window_id,
            debounce: ReactivationDebounce::default(),
        })
    }

    /// Convenience constructor: build the guard from a GPUI window handle,
    /// yielding `None` on non-X11 backends (where the guard should stay off).
    #[must_use]
    #[cfg(target_os = "linux")]
    pub fn from_window_handle(handle_source: &impl HasWindowHandle) -> Option<Self> {
        Self::new(xcb_window_id(handle_source)?)
    }

    /// Keep the cross-platform client shell inert when X11 is unavailable.
    #[must_use]
    #[cfg(not(target_os = "linux"))]
    pub fn from_window_handle<T>(_handle_source: &T) -> Option<Self> {
        None
    }

    /// Refresh cached state by querying `_NET_ACTIVE_WINDOW`.
    ///
    /// Call from a periodic callback so the guard has an up-to-date picture of
    /// whether a compositor overlay is active. Does not itself suppress input.
    pub fn poll(&mut self) {
        #[cfg(target_os = "linux")]
        if self.query_is_active() {
            self.debounce.note_active(Instant::now());
        } else {
            self.debounce.note_inactive();
        }
        #[cfg(not(target_os = "linux"))]
        match self.never {}
    }

    /// Clear the reactivation debounce on a genuine focus event.
    pub fn clear_reactivation_debounce(&mut self) {
        #[cfg(target_os = "linux")]
        self.debounce.clear();
        #[cfg(not(target_os = "linux"))]
        match self.never {}
    }

    /// Returns `true` when keyboard input should be suppressed: either our
    /// window is not the current `_NET_ACTIVE_WINDOW`, or it just became active
    /// again within [`REACTIVATION_DEBOUNCE`].
    pub fn should_suppress_key(&mut self) -> bool {
        #[cfg(not(target_os = "linux"))]
        match self.never {}

        #[cfg(target_os = "linux")]
        {
            let is_active = self.query_is_active();
            self.debounce.observe(is_active, Instant::now())
        }
    }

    /// Query `_NET_ACTIVE_WINDOW` and return whether it matches our window.
    /// Assumes active on any X11 error so a transient failure never wedges
    /// input.
    #[cfg(target_os = "linux")]
    fn query_is_active(&self) -> bool {
        let Ok(cookie) = self.conn.get_property(
            false,
            self.root,
            self.net_active_window,
            AtomEnum::WINDOW,
            0,
            1,
        ) else {
            return true;
        };

        let Ok(reply) = cookie.reply() else {
            return true;
        };

        reply.value32().and_then(|mut iter| iter.next()) == Some(self.our_window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#X11 focus guard#Inactive window suppresses input]]
    #[test]
    fn inactive_window_suppresses() {
        let mut d = ReactivationDebounce::default();
        let now = Instant::now();
        assert!(d.observe(false, now));
    }

    // @lat: [[test#X11 focus guard#Reactivation debounce suppresses stray keys]]
    #[test]
    fn reactivation_debounce_suppresses_then_clears() {
        let mut d = ReactivationDebounce::default();
        let t0 = Instant::now();
        // Went inactive, then active: the transition arms the debounce.
        assert!(d.observe(false, t0));
        assert!(d.observe(true, t0));
        // Still within the debounce window.
        assert!(d.observe(true, t0 + Duration::from_millis(100)));
        // Past the debounce window: input flows again.
        assert!(!d.observe(true, t0 + REACTIVATION_DEBOUNCE + Duration::from_millis(1)));
    }

    // @lat: [[test#X11 focus guard#Steady active window allows input]]
    #[test]
    fn steady_active_window_allows_input() {
        let mut d = ReactivationDebounce::default();
        let now = Instant::now();
        assert!(!d.observe(true, now));
    }

    // @lat: [[test#X11 focus guard#Genuine focus event clears debounce]]
    #[test]
    fn genuine_focus_event_clears_debounce() {
        let mut d = ReactivationDebounce::default();
        let t0 = Instant::now();
        assert!(d.observe(false, t0));
        d.clear();
        // After a real focus event the debounce is gone; active is not suppressed.
        assert!(!d.observe(true, t0));
    }

    // @lat: [[test#X11 focus guard#Poll transition arms debounce]]
    #[test]
    fn poll_transition_arms_debounce() {
        let mut d = ReactivationDebounce::default();
        let t0 = Instant::now();
        d.note_inactive();
        d.note_active(t0);
        // A key observed just after the poll-detected reactivation is suppressed.
        assert!(d.observe(true, t0 + Duration::from_millis(50)));
    }
}
