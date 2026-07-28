//! X11 active-window guard.
//!
//! Queries `_NET_ACTIVE_WINDOW` on the root window to detect when a compositor
//! overlay (e.g. GNOME Screenshot) obscures this window without sending an X11
//! focus event.  The guard also debounces key events for a short period after
//! re-activation to catch stray keystrokes (e.g. Enter to confirm a screenshot)
//! that arrive just after the overlay closes.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// Debounce window after the compositor overlay dismisses.
const REACTIVATION_DEBOUNCE: Duration = Duration::from_millis(300);

/// Polls `_NET_ACTIVE_WINDOW` and suppresses keyboard input while our window
/// is not the active one (or was not active very recently).
pub struct X11FocusGuard {
    conn: RustConnection,
    root: u32,
    net_active_window: u32,
    our_window: u32,
    /// Whether the window has been inactive since the last re-activation.
    was_inactive: bool,
    /// When the inactive→active transition was first detected (by `poll` or
    /// `should_suppress_key`).  The debounce is measured from this moment.
    reactivated_at: Option<Instant>,
}

impl X11FocusGuard {
    /// Attempt to open an independent X11 connection and prepare the guard.
    ///
    /// Returns `None` when X11 is unavailable (e.g. pure Wayland) or when the
    /// connection/atom intern fails for any reason.
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
            was_inactive: false,
            reactivated_at: None,
        })
    }

    /// Refresh cached state by querying `_NET_ACTIVE_WINDOW`.
    ///
    /// Call from `about_to_wait` (or similar periodic callback) so the guard
    /// has an up-to-date picture of whether a compositor overlay is active.
    pub fn poll(&mut self) {
        if !self.query_is_active() {
            self.was_inactive = true;
        } else if self.was_inactive {
            // Transition: inactive → active detected by poll.  Start the
            // debounce from this moment so stray keystrokes that arrive
            // shortly after are still suppressed.
            self.was_inactive = false;
            self.reactivated_at = Some(Instant::now());
        }
    }

    /// Clear the reactivation debounce.
    ///
    /// Call when the window receives a real focus event (`Focused(true)`).
    /// Compositor overlays don't send focus events, so this only fires for
    /// genuine focus transitions where the debounce should not apply.
    pub fn clear_reactivation_debounce(&mut self) {
        self.was_inactive = false;
        self.reactivated_at = None;
    }

    /// Returns `true` when keyboard input should be suppressed.
    ///
    /// Suppression occurs when:
    /// 1. Our window is not the current `_NET_ACTIVE_WINDOW`, OR
    /// 2. Our window *just* became active again (within [`REACTIVATION_DEBOUNCE`]).
    pub fn should_suppress_key(&mut self) -> bool {
        if !self.query_is_active() {
            self.was_inactive = true;
            return true;
        }

        // Transition: inactive → active.  Start the debounce from this
        // exact moment so the stray keystroke that triggered this call is
        // always within the window.
        if self.was_inactive {
            self.was_inactive = false;
            self.reactivated_at = Some(Instant::now());
        }

        // Within the post-reactivation debounce window?
        if let Some(t) = self.reactivated_at {
            if t.elapsed() < REACTIVATION_DEBOUNCE {
                return true;
            }
            self.reactivated_at = None;
        }

        false
    }

    /// Query `_NET_ACTIVE_WINDOW` and return whether it matches our window.
    fn query_is_active(&self) -> bool {
        let Ok(cookie) = self.conn.get_property(
            false,
            self.root,
            self.net_active_window,
            AtomEnum::WINDOW,
            0,
            1,
        ) else {
            return true; // assume active on error
        };

        let Ok(reply) = cookie.reply() else {
            return true;
        };

        // Property absent or empty → no active window → overlay likely active.
        reply.value32().and_then(|mut iter| iter.next()) == Some(self.our_window)
    }
}
