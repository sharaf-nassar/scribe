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

use crate::window_state::{ObservedWindowState, WindowState};

/// Read the live window state off the window manager, falling back to GPUI.
///
/// `_NET_WM_STATE` is WM-owned: EWMH 5.7 says `_NET_WM_STATE_HIDDEN` "is a
/// function of some other aspect of the window such as minimization, rather
/// than an independent state", so it is only ever read here, never written.
/// ICCCM `WM_STATE == IconicState` is the pre-EWMH equivalent and answers for a
/// window manager that does not publish the atom.
///
/// This read exists because GPUI cannot answer the question: it exposes no
/// `is_minimized()` at all, and its X11 `is_maximized()` is
/// `!hidden && maximized_vertical && maximized_horizontal`, so a window
/// minimized from maximized reports plain windowed — quitting there used to
/// persist the wrong state and bring the window back unmaximized. `fallback`
/// (GPUI's own answer) is returned off X11 and on any protocol failure, which
/// is what keeps Wayland and macOS behaving as they did.
#[must_use]
pub fn observed_window_state(
    window: &Window,
    fallback: ObservedWindowState,
) -> ObservedWindowState {
    x11::observed_window_state(window).unwrap_or(fallback)
}

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

/// `RandR` connector name of the monitor the window is on right now.
///
/// Unlike [`persisted_monitor_name`] there is no display-UUID fallback: this is
/// the post-condition check restore compares against the record's
/// `monitor_name`, and only the connector name is comparable to it.
#[must_use]
pub fn window_monitor_name(window: &Window) -> Option<String> {
    x11::window_monitor_name(window)
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
/// The move goes out as an EWMH `_NET_MOVERESIZE_WINDOW` with `StaticGravity`,
/// which is the standard way to place a window *including its decorations*
/// without knowing their size (EWMH 4.2). A plain `ConfigureRequest` is
/// ambiguous about frame versus client origin, which is what used to leave a
/// titlebar-sized residual to measure and correct. Window managers that do not
/// advertise the message fall back to that `ConfigureRequest`.
///
/// A window in `state` is placed by unmaximizing it first: the window manager
/// owns a maximized or fullscreen window's geometry, so a move sent while the
/// state is set is ignored. `_NET_WM_STATE_REMOVE` of the atoms that own the
/// placement, the move, then `_NET_WM_STATE_ADD` of the same atoms puts the
/// window back in its state on the monitor the move chose. EWMH 5.7 allows one
/// message to carry both maximize atoms "specifically to allow both horizontal
/// and vertical maximization to be altered together", so the window never
/// passes through a half-maximized frame.
///
/// Returns whether a request was actually sent (false off X11). Where the
/// window ends up is still the window manager's answer, not ours: struts,
/// snapping, or an off-screen clamp can move it further, which is why the
/// caller verifies the monitor it landed on.
pub fn apply_saved_position(window: &Window, x: i32, y: i32, state: WindowState) -> bool {
    x11::move_window(window, x, y, state)
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::cell::OnceCell;

    use gpui::Window;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{
        Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask,
    };
    use x11rb::rust_connection::RustConnection;

    use crate::window_state::{ObservedWindowState, WindowState};
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

    /// The window's EWMH state, or `None` when the window manager publishes no
    /// `_NET_WM_STATE` for it (no EWMH, or not X11 at all).
    pub(super) fn observed_window_state(window: &Window) -> Option<ObservedWindowState> {
        let xid = xcb_window_id(window)?;
        with_connection(|conn| {
            let property = intern(conn, b"_NET_WM_STATE")?;
            let reply = conn
                .get_property(false, xid, property, AtomEnum::ATOM, 0, 64)
                .ok()?
                .reply()
                .ok()?;
            // Absent (format 0) rather than merely empty: no EWMH answer to
            // read, so the caller keeps GPUI's.
            let set: Vec<Atom> = reply.value32()?.collect();
            let holds = |name: &[u8]| intern(conn, name).is_some_and(|atom| set.contains(&atom));
            let maximized =
                || holds(b"_NET_WM_STATE_MAXIMIZED_VERT") && holds(b"_NET_WM_STATE_MAXIMIZED_HORZ");
            let visible = holds(b"_NET_WM_STATE_FULLSCREEN")
                .then_some(WindowState::Fullscreen)
                .or_else(|| maximized().then_some(WindowState::Maximized))
                .unwrap_or_default();
            let hidden = holds(b"_NET_WM_STATE_HIDDEN") || wm_state_is_iconic(conn, xid);
            Some(ObservedWindowState::from_wm_state(hidden, visible))
        })
    }

    /// ICCCM 4.1.3.1 `WM_STATE`, whose first word is the state and whose type
    /// atom is the property atom. `IconicState` is the pre-EWMH "minimized".
    fn wm_state_is_iconic(conn: &RustConnection, xid: u32) -> bool {
        const ICONIC_STATE: u32 = 3;
        let Some(property) = intern(conn, b"WM_STATE") else { return false };
        conn.get_property(false, xid, property, property, 0, 2)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.value32()?.next())
            .is_some_and(|state| state == ICONIC_STATE)
    }

    /// An already-interned atom by name, or `None` when nothing has interned it
    /// (`only_if_exists`), which for a WM-owned atom means "not in that state".
    fn intern(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
        let atom = conn.intern_atom(true, name).ok()?.reply().ok()?.atom;
        (atom != 0).then_some(atom)
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
    /// Only a request, answered asynchronously: the reply is deliberately NOT
    /// read back here, because a `get_geometry` issued in the same breath
    /// reports where the window still is. Where it landed is observed later,
    /// from the window's own bounds change — see
    /// `TerminalView::verify_restored_position`.
    ///
    /// A window whose `state` hands its geometry to the window manager is
    /// unmaximized around the move and put back afterwards; the three messages
    /// reach the window manager in the order they are sent.
    pub(super) fn move_window(window: &Window, x: i32, y: i32, state: WindowState) -> bool {
        let Some(xid) = xcb_window_id(window) else { return false };
        with_connection(|conn| {
            let root = conn.get_geometry(xid).ok()?.reply().ok()?.root;
            let held = placement_atoms(conn, root, state);
            let lifted = set_wm_state(conn, root, xid, WM_STATE_REMOVE, &held);
            let moved =
                move_resize_window(conn, root, xid, x, y) || configure_position(conn, xid, x, y);
            // Put back even when the move failed: a lifted state that is not
            // restored would leave the window unmaximized.
            if lifted {
                set_wm_state(conn, root, xid, WM_STATE_ADD, &held);
            }
            moved.then_some(())
        })
        .is_some()
    }

    /// The ICCCM `ConfigureRequest` fallback, for a window manager that does
    /// not advertise `_NET_MOVERESIZE_WINDOW`.
    fn configure_position(conn: &RustConnection, xid: u32, x: i32, y: i32) -> bool {
        let aux = x11rb::protocol::xproto::ConfigureWindowAux::new().x(x).y(y);
        conn.configure_window(xid, &aux).is_ok_and(|cookie| cookie.check().is_ok())
    }

    const WM_STATE_REMOVE: u32 = 0;
    const WM_STATE_ADD: u32 = 1;
    /// EWMH source indication for "a normal application" (2 is a pager).
    const SOURCE_APPLICATION: u32 = 1;

    /// The `_NET_WM_STATE` atoms that take a window's placement away from the
    /// client, empty when `state` leaves the client its own geometry or the
    /// window manager does not advertise the whole set.
    ///
    /// All-or-nothing on purpose: removing one of the two maximize atoms would
    /// leave a half-maximized window behind.
    fn placement_atoms(conn: &RustConnection, root: u32, state: WindowState) -> Vec<Atom> {
        let names: &[&[u8]] = match state {
            WindowState::Maximized => {
                &[b"_NET_WM_STATE_MAXIMIZED_VERT", b"_NET_WM_STATE_MAXIMIZED_HORZ"]
            }
            WindowState::Fullscreen => &[b"_NET_WM_STATE_FULLSCREEN"],
            WindowState::Windowed | WindowState::Minimized => &[],
        };
        names
            .iter()
            .map(|name| supported_atom(conn, root, name))
            .collect::<Option<Vec<Atom>>>()
            .unwrap_or_default()
    }

    /// Add or remove up to two `_NET_WM_STATE` properties in one client
    /// message, the pair EWMH 5.7 allows so both maximize axes change together.
    /// Reports whether the message went out; `false` for an empty atom set.
    fn set_wm_state(
        conn: &RustConnection,
        root: u32,
        xid: u32,
        action: u32,
        atoms: &[Atom],
    ) -> bool {
        if atoms.is_empty() {
            return false;
        }
        let Some(message) = supported_atom(conn, root, b"_NET_WM_STATE") else { return false };
        let first = atoms.first().copied().unwrap_or(0);
        let second = atoms.get(1).copied().unwrap_or(0);
        send_to_wm(conn, root, xid, message, [action, first, second, SOURCE_APPLICATION, 0])
    }

    /// Send a client message the window manager, not the X server, answers:
    /// addressed to the root window, with the substructure-redirect mask.
    fn send_to_wm(
        conn: &RustConnection,
        root: u32,
        xid: u32,
        message: Atom,
        data: [u32; 5],
    ) -> bool {
        let event = ClientMessageEvent::new(32, xid, message, data);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .is_ok_and(|cookie| cookie.check().is_ok())
    }

    /// Place the window with EWMH `_NET_MOVERESIZE_WINDOW` and `StaticGravity`,
    /// reporting whether the message went out.
    ///
    /// `StaticGravity` makes the coordinates the *client* origin whatever frame
    /// the window manager drew around it, so there is no decoration residual to
    /// measure afterwards. `false` when the window manager does not advertise
    /// the message, which leaves the caller on the `ConfigureRequest` path.
    fn move_resize_window(conn: &RustConnection, root: u32, xid: u32, x: i32, y: i32) -> bool {
        // Gravity `StaticGravity` (0xa) in the low byte, bits 8 and 9 marking x
        // and y as present, bit 12 the "application" source indication.
        const FLAGS: u32 = 0x0000_130a;
        let Some(message) = supported_atom(conn, root, b"_NET_MOVERESIZE_WINDOW") else {
            return false;
        };
        send_to_wm(conn, root, xid, message, [FLAGS, x.cast_unsigned(), y.cast_unsigned(), 0, 0])
    }

    /// The atom for `name`, but only when the window manager lists it in
    /// `_NET_SUPPORTED`.
    fn supported_atom(conn: &RustConnection, root: u32, name: &[u8]) -> Option<Atom> {
        let wanted = intern(conn, name)?;
        let supported = intern(conn, b"_NET_SUPPORTED")?;
        let reply = conn
            .get_property(false, root, supported, AtomEnum::ATOM, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.any(|atom| atom == wanted).then_some(wanted)
    }
}

#[cfg(not(target_os = "linux"))]
mod x11 {
    use gpui::Window;

    use crate::window_state::{ObservedWindowState, WindowState};

    pub(super) fn observed_window_state(_window: &Window) -> Option<ObservedWindowState> {
        None
    }

    pub(super) fn window_monitor_name(_window: &Window) -> Option<String> {
        None
    }

    pub(super) fn connected_monitor_names() -> Vec<String> {
        Vec::new()
    }

    pub(super) fn move_window(_window: &Window, _x: i32, _y: i32, _state: WindowState) -> bool {
        false
    }
}
