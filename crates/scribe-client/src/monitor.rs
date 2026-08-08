//! Persistent monitor identity for window-geometry state.
//!
//! GPUI's X11 backend hardcodes `PlatformDisplay::uuid()` to the nil UUID
//! (`X11Display::new` in `gpui_linux`), so persisting that value stamps every
//! window with the same unknown monitor. This module restores the identity the
//! legacy winit client persisted: the `RandR` connector name (`"DP-4"`) of the
//! monitor under the window. The fallback chain is connector name → GPUI
//! display UUID where it is real (Wayland derives a stable v5 UUID from the
//! output name; macOS uses the `CGDisplay` UUID) → `None` (unknown). Restore
//! gates saved positions on this identity via
//! [`crate::window_state::gate_position_on_monitor`].

use gpui::{App, Window};

/// Resolve the monitor identity persisted with window geometry.
///
/// Fallback chain: X11 `RandR` connector name of the monitor containing the
/// window → GPUI display UUID when it is not the nil placeholder → `None`,
/// which restore treats as "unknown monitor → default placement".
#[must_use]
pub fn persisted_monitor_name(window: &Window, cx: &App) -> Option<String> {
    x11::window_monitor_name(window).or_else(|| {
        window
            .display(cx)
            .and_then(|display| display.uuid().ok())
            .filter(|uuid| !uuid.is_nil())
            .map(|uuid| uuid.to_string())
    })
}

/// Connector names of every currently connected monitor, best effort.
///
/// Empty when the platform cannot enumerate monitors (pure Wayland, macOS,
/// `RandR` failure). Callers treat an empty list as "cannot verify" and keep the
/// saved position, so platforms without `RandR` behave as they did before the
/// monitor gate existed.
#[must_use]
pub fn connected_monitor_names() -> Vec<String> {
    x11::connected_monitor_names()
}

/// Re-assert a restored window's saved position once it is on screen.
///
/// The bounds handed to `open_window` are only a *hint*: GPUI's X11 backend
/// passes them to `create_window` but sets no `USPosition`/`PPosition` size
/// hint, and under ICCCM a window without one is placed entirely at the window
/// manager's discretion. Mutter takes that discretion and puts every new window
/// on the active monitor, which is why restored windows all came back on one
/// screen no matter what their geometry record said. GPUI exposes
/// `Window::resize` but no way to move a window, so the position is applied
/// through the same X11 connection this module already keeps for `RandR`.
///
/// Returns whether a request was actually sent (false off X11). Where the
/// window ends up is the window manager's answer, not ours: a reparenting frame
/// offsets it, and struts, snapping, or an off-screen clamp can move it further.
/// The caller measures the result from the window's next bounds change and
/// corrects once.
pub fn apply_saved_position(window: &Window, x: i32, y: i32) -> bool {
    x11::move_window(window, x, y)
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::cell::OnceCell;

    use gpui::Window;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{Atom, ConnectionExt as _};
    use x11rb::rust_connection::RustConnection;

    use crate::x11_focus::xcb_window_id;

    thread_local! {
        /// One X11 connection per thread, opened lazily and reused: bounds
        /// observations fire per frame during a drag, so reconnecting each
        /// time would be wasteful. A failed connect (pure Wayland, headless)
        /// is cached as `None` so the failure is not retried per frame.
        static CONNECTION: OnceCell<Option<RustConnection>> = const { OnceCell::new() };
    }

    fn with_connection<R>(f: impl FnOnce(&RustConnection) -> Option<R>) -> Option<R> {
        CONNECTION.with(|cell| {
            cell.get_or_init(|| x11rb::connect(None).ok().map(|(conn, _screen)| conn))
                .as_ref()
                .and_then(f)
        })
    }

    /// `RandR` connector name of the monitor containing the window center, or
    /// `None` off X11 or on any protocol failure.
    pub(super) fn window_monitor_name(window: &Window) -> Option<String> {
        let xid = xcb_window_id(window)?;
        with_connection(|conn| {
            let geometry = conn.get_geometry(xid).ok()?.reply().ok()?;
            let origin = conn.translate_coordinates(xid, geometry.root, 0, 0).ok()?.reply().ok()?;
            let center_x = i32::from(origin.dst_x) + i32::from(geometry.width) / 2;
            let center_y = i32::from(origin.dst_y) + i32::from(geometry.height) / 2;
            let monitors = conn.randr_get_monitors(geometry.root, true).ok()?.reply().ok()?;
            let containing = monitors.monitors.iter().find(|monitor| {
                let x = i32::from(monitor.x);
                let y = i32::from(monitor.y);
                center_x >= x
                    && center_x < x + i32::from(monitor.width)
                    && center_y >= y
                    && center_y < y + i32::from(monitor.height)
            });
            // A window mid-drag or partially off-screen still belongs to the
            // primary (or sole) monitor rather than to no monitor at all.
            let monitor = containing
                .or_else(|| monitors.monitors.iter().find(|monitor| monitor.primary))
                .or_else(|| monitors.monitors.first())?;
            atom_name(conn, monitor.name)
        })
    }

    /// Connector names of all active `RandR` monitors across every X screen.
    pub(super) fn connected_monitor_names() -> Vec<String> {
        with_connection(|conn| {
            let roots: Vec<u32> = conn.setup().roots.iter().map(|screen| screen.root).collect();
            Some(roots.iter().flat_map(|&root| screen_monitor_names(conn, root)).collect())
        })
        .unwrap_or_default()
    }

    /// Connector names of one screen's active `RandR` monitors, best effort.
    fn screen_monitor_names(conn: &RustConnection, root: u32) -> Vec<String> {
        let Ok(cookie) = conn.randr_get_monitors(root, true) else { return Vec::new() };
        let Ok(reply) = cookie.reply() else { return Vec::new() };
        reply.monitors.iter().filter_map(|monitor| atom_name(conn, monitor.name)).collect()
    }

    fn atom_name(conn: &RustConnection, atom: Atom) -> Option<String> {
        let reply = conn.get_atom_name(atom).ok()?.reply().ok()?;
        String::from_utf8(reply.name).ok()
    }

    /// Ask the window manager to put a mapped window at a root-relative
    /// position.
    ///
    /// Only a request. The reply is deliberately NOT read back here: a
    /// `ConfigureRequest` is answered asynchronously, so a `get_geometry` issued
    /// in the same breath reports where the window still is, and a correction
    /// computed from that reading sends the window somewhere arbitrary. Where it
    /// landed is observed later, from the window's own bounds change — see
    /// `TerminalView::correct_restored_position`.
    pub(super) fn move_window(window: &Window, x: i32, y: i32) -> bool {
        let Some(xid) = xcb_window_id(window) else { return false };
        with_connection(|conn| {
            let aux = x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y);
            conn.configure_window(xid, &aux).ok()?.check().ok()?;
            Some(())
        })
        .is_some()
    }
}

#[cfg(not(target_os = "linux"))]
mod x11 {
    use gpui::Window;

    pub(super) fn window_monitor_name(_window: &Window) -> Option<String> {
        None
    }

    pub(super) fn connected_monitor_names() -> Vec<String> {
        Vec::new()
    }

    pub(super) fn move_window(_window: &Window, _x: i32, _y: i32) -> bool {
        false
    }
}
