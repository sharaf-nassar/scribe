//! Persistent window geometry state for the GPUI client.
//!
//! Stores window position, size, [`WindowState`], font zoom level, and monitor name in
//! `$XDG_STATE_HOME/scribe/windows/<window_id>.toml` (one file per window to
//! avoid save races between window processes). Ported from the legacy winit
//! client's `window_state.rs`; the winit `capture`/`apply` glue is replaced by
//! GPUI-native bounds conversion (the shell bead) since GPUI owns the window.
//! `monitor_name` is the `RandR` connector name resolved by [`crate::monitor`]
//! (GPUI's X11 display uuid is a nil placeholder — [`NIL_MONITOR_ID`]), and
//! [`clamp_geometry_to_layout`] moves a saved rect back into the monitor
//! layout the window is about to reopen on.
//!
//! It also adds the first-launch **geometry-compat normalization**
//! ([`normalize_legacy_geometry`]): geometry persisted by the OS-decorated old
//! client restores mis-inset under the new custom titlebar, so the first launch
//! after cutover clamps the size and applies a titlebar inset once, recording
//! `titlebar_normalized` so it never runs twice.

use std::path::PathBuf;

use gpui::{Bounds, Pixels, Point, Size, WindowBounds, point, px, size};
use scribe_common::app::current_state_dir;
use scribe_common::ids::WindowId;
use serde::{Deserialize, Serialize};

use crate::restore_replay::round_positive_f32_to_u16;

/// Minimum accepted window edge, in logical pixels.
pub const MIN_WINDOW_EDGE: u32 = 40;
/// Maximum accepted window edge, in logical pixels.
pub const MAX_WINDOW_EDGE: u32 = 16384;

/// Height, in logical pixels, of the client-drawn custom titlebar. The
/// geometry-compat normalization grows a legacy window by this amount so the
/// terminal content area below the titlebar keeps the size it had under the old
/// client's OS-drawn decoration.
pub const CUSTOM_TITLEBAR_HEIGHT: u32 = 36;

/// Errors that can occur during state persistence.
///
/// These are always handled gracefully by callers (logged, never fatal).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// No XDG state directory could be determined.
    #[error("could not determine XDG state directory")]
    NoStateDir,
    /// Filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// TOML serialization failure.
    #[error("TOML serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// How a window was last displayed.
///
/// Replaces the old `maximized: bool`, which could represent neither a
/// minimized nor a fullscreen window. Records written before this existed carry
/// `maximized = true|false` instead; [`WindowRegistry::load_saved`] folds that
/// into [`Self::Maximized`]/[`Self::Windowed`] on the way in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowState {
    /// A normal window with a position and a size of its own.
    #[default]
    Windowed,
    /// Filling the work area, with the window manager owning its geometry.
    Maximized,
    /// Iconified. Whatever it was before is kept in
    /// [`WindowGeometry::restore_state`].
    Minimized,
    /// Filling the whole monitor, decorations and struts included.
    Fullscreen,
}

/// A live window's state, as the platform reports it right now.
///
/// Split in two because minimization is orthogonal to the rest: the window
/// manager keeps a minimized window's maximized and fullscreen bits set, and
/// losing them is what made "minimize a maximized window, then quit" come back
/// windowed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservedWindowState {
    /// What the window is now.
    pub state: WindowState,
    /// What unminimizing it would return to. Only meaningful when `state` is
    /// [`WindowState::Minimized`].
    pub restore_state: WindowState,
}

impl ObservedWindowState {
    /// Combine the window manager's hidden bit with the state underneath it.
    ///
    /// Hidden wins, because it is the state the window is actually in; what it
    /// hides becomes the restore state rather than being thrown away, which is
    /// the whole point of keeping the two apart. `visible` is the caller's read
    /// of the remaining EWMH bits, taken fullscreen-before-maximized: a window
    /// manager leaves the maximized bits set underneath a fullscreen window, so
    /// reading those first would lose the fullscreen.
    #[must_use]
    pub fn from_wm_state(hidden: bool, visible: WindowState) -> Self {
        if hidden {
            Self { state: WindowState::Minimized, restore_state: visible }
        } else {
            Self { state: visible, restore_state: WindowState::Windowed }
        }
    }
}

/// The windowed rect a maximized, fullscreen, or minimized window returns to.
///
/// Kept separately from the record's own rect because that one tracks the
/// window as it is: for a maximized window it is the work area, and EWMH 5.7
/// makes restoring the pre-fullscreen geometry the window manager's job, which
/// it can only do if it was handed the rect to restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRect {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: u32,
    pub height: u32,
}

/// Persisted window geometry and display state.
///
/// Position fields are `Option` because Wayland does not expose window
/// positions — storing `None` prevents a bogus `(0, 0)` from being applied
/// when the user later runs on X11. `titlebar_normalized` defaults to `false`
/// so legacy files written by the old client trigger the one-time
/// geometry-compat normalization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: u32,
    pub height: u32,
    /// How the window was displayed. Absent in pre-[`WindowState`] records,
    /// which carry `maximized` instead.
    #[serde(default)]
    pub state: WindowState,
    /// What unminimizing the window would return to; only read when `state` is
    /// [`WindowState::Minimized`].
    #[serde(default)]
    pub restore_state: WindowState,
    pub monitor_name: Option<String>,
    /// EWMH `_NET_WM_DESKTOP`: the virtual desktop the window was on, with
    /// `0xFFFF_FFFF` meaning "all desktops". `None` when the platform or window
    /// manager does not publish one, and in records written before this field
    /// existed.
    #[serde(default)]
    pub desktop: Option<u32>,
    /// Whether the geometry-compat titlebar inset has already been applied.
    /// Absent in legacy files (`serde(default)` → `false`).
    #[serde(default)]
    pub titlebar_normalized: bool,
    /// Font zoom level the window was last at, as the point *delta* over
    /// `appearance.font_size` that [`crate::zoom::ZoomState`] holds — never the
    /// resulting size, so a later config edit rebases the delta instead of
    /// being overridden by a size captured against the old one. `serde(default)`
    /// → `0` (unzoomed) for records written before this field existed, and
    /// [`crate::zoom::ZoomState::at_level`] re-clamps whatever is read back.
    #[serde(default)]
    pub zoom: i8,
    /// Legacy `maximized = true|false`, read from pre-[`WindowState`] records
    /// and folded into `state` by [`adopt_legacy_state`]. Never written back.
    #[serde(default, rename = "maximized", skip_serializing)]
    legacy_maximized: Option<bool>,
    /// Declared last: it serializes as a TOML table, and TOML cannot carry a
    /// bare key after one.
    #[serde(default)]
    pub restore_rect: Option<SavedRect>,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            width: 1200,
            height: 800,
            state: WindowState::Windowed,
            restore_state: WindowState::Windowed,
            monitor_name: None,
            desktop: None,
            // A freshly-created default is already in the new coordinate system.
            titlebar_normalized: true,
            zoom: 0,
            legacy_maximized: None,
            restore_rect: None,
        }
    }
}

impl WindowGeometry {
    /// The state the window is opened in, with minimization resolved away.
    ///
    /// A window cannot be *created* minimized through GPUI — the platform
    /// window is mapped inside `Window::new` — so restore opens it in the state
    /// it would unminimize to and issues the minimize request afterwards.
    #[must_use]
    pub fn effective_state(&self) -> WindowState {
        match (self.state, self.restore_state) {
            // A record claiming to unminimize into minimization is nonsense
            // from a hand-edited or truncated file; windowed is the safe read.
            (WindowState::Minimized, WindowState::Minimized) => WindowState::Windowed,
            (WindowState::Minimized, restore) => restore,
            (state, _) => state,
        }
    }

    /// The same record with its virtual desktop filled in.
    ///
    /// The desktop is not part of the bounds conversion, so
    /// [`geometry_from_bounds`] leaves it to the caller; this exists because
    /// the private legacy-`maximized` field blocks struct-update syntax from
    /// outside this module and adding a sixth parameter pushes
    /// `geometry_from_bounds` past Clippy's argument limit.
    #[must_use]
    pub fn on_desktop(self, desktop: Option<u32>) -> Self {
        Self { desktop, ..self }
    }

    /// The same record with the live font zoom level filled in.
    ///
    /// A sibling of [`Self::on_desktop`], for the same two reasons: the zoom is
    /// not part of the bounds conversion, and the private legacy-`maximized`
    /// field blocks struct-update syntax from outside this module.
    #[must_use]
    pub fn at_zoom(self, zoom: i8) -> Self {
        Self { zoom, ..self }
    }

    /// The origin a restore re-asserts, or `None` when none was captured.
    ///
    /// A maximized or fullscreen window has a placement of its own even though
    /// the window manager owns its size: the monitor follows the origin. The
    /// pre-maximize rect is the one to aim at — it is both on that monitor and
    /// where the window returns to when it is unmaximized around the move — and
    /// the record's own origin answers for a legacy record that never captured
    /// one.
    #[must_use]
    pub fn restore_origin(&self) -> Option<(i32, i32)> {
        self.windowed_rect().and_then(|rect| rect.x.zip(rect.y)).or_else(|| self.x.zip(self.y))
    }

    /// The windowed rect to return to, which for a windowed record is its own.
    ///
    /// This is what makes the pre-maximize rect survive: the capture that first
    /// sees the window maximized reads it off the previous (windowed) record,
    /// and every later capture carries it forward unchanged.
    #[must_use]
    fn windowed_rect(&self) -> Option<SavedRect> {
        if self.state == WindowState::Windowed {
            Some(SavedRect { x: self.x, y: self.y, width: self.width, height: self.height })
        } else {
            self.restore_rect
        }
    }
}

/// Fold a pre-[`WindowState`] record's `maximized` bool into `state`.
///
/// Runs on every load rather than inside [`normalize_legacy_geometry`], which
/// short-circuits on `titlebar_normalized`: records written by the intervening
/// client are already normalized *and* still carry the bool.
#[must_use]
fn adopt_legacy_state(geom: WindowGeometry) -> WindowGeometry {
    let state = match geom.legacy_maximized {
        Some(true) if geom.state == WindowState::Windowed => WindowState::Maximized,
        _ => geom.state,
    };
    WindowGeometry { state, legacy_maximized: None, ..geom }
}

/// The nil UUID string GPUI's X11 backend reports as its display "uuid".
///
/// v0.1.0 cutover clients persisted this verbatim as `monitor_name`, so every
/// pre-connector-name record on X11 carries it. It is not a connector name and
/// can never match one, so restore treats it as "monitor identity unknown" and
/// skips the post-move monitor check rather than warning on every such start.
/// The record self-heals to a `RandR` connector name on the next geometry
/// capture.
pub const NIL_MONITOR_ID: &str = "00000000-0000-0000-0000-000000000000";

/// A connected monitor and the area of it a window may occupy.
///
/// The rect is the *work area*, not the whole monitor: the `RandR` rect minus
/// the struts panels and docks reserve (`_NET_WORKAREA`), so a window clamped
/// into it clears the panel instead of merely landing on screen.
/// [`crate::monitor::connected_monitors`] resolves the list, and an empty one
/// means the platform cannot enumerate monitors at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorWorkArea {
    /// `RandR` connector name — the identity a record's `monitor_name` holds.
    pub name: String,
    /// Root-relative work-area origin and size, in logical pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorWorkArea {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(i32_to_logical_px(self.x)), px(i32_to_logical_px(self.y))),
            size: size(px(u32_to_logical_px(self.width)), px(u32_to_logical_px(self.height))),
        }
    }
}

/// Move a saved record into the monitor layout it is about to reopen on.
///
/// This replaces the older gate, which dropped `x`/`y` outright once the record
/// named a monitor that was verifiably gone. Dropping the position hands the
/// window back to the window manager's default placement *and* keeps a size the
/// remaining screen cannot hold — a 3840-wide window reopening on a 1920-wide
/// screen, with whatever the window manager settled on persisted afterwards.
/// What toolkits do instead, and what Zed is missing in zed-industries/zed#12521
/// and #47231, is clamp the saved rect into the current layout: the window comes
/// back as close to where the user left it as the layout allows, and reachable.
///
/// A rect that still touches a work area is clamped into the union of the areas
/// it touches, so a window deliberately spanning two monitors keeps spanning
/// them and only an oversized one is shrunk. A rect that touches none is moved
/// onto the monitor nearest its centre and adopts that monitor's name, which is
/// also what stops `TerminalView::verify_restored_position` from reporting the
/// deliberate move as the window manager picking the wrong screen.
///
/// An empty `connected` means the platform cannot enumerate monitors (macOS,
/// pure Wayland, no `RandR`): nothing is verifiable, so the record is returned
/// untouched, exactly as those platforms behaved before any of this existed.
#[must_use]
pub fn clamp_geometry_to_layout(
    geom: &WindowGeometry,
    connected: &[MonitorWorkArea],
) -> WindowGeometry {
    // A record with no origin — captured where the platform hides window
    // positions — opens at GPUI's default placement, which lands on the primary
    // monitor (`RandR` lists it first), so that is where its size is measured.
    let Some(default_placement) = connected.first().map(MonitorWorkArea::bounds) else {
        return geom.clone();
    };
    let own = SavedRect { x: geom.x, y: geom.y, width: geom.width, height: geom.height };
    let (fitted, moved_to) = clamp_saved_rect(own, connected, default_placement);
    let clamped = WindowGeometry {
        x: fitted.x,
        y: fitted.y,
        width: fitted.width,
        height: fitted.height,
        monitor_name: moved_to
            .map_or_else(|| geom.monitor_name.clone(), |area| Some(area.name.clone())),
        restore_rect: geom
            .restore_rect
            .map(|rect| clamp_saved_rect(rect, connected, default_placement).0),
        ..geom.clone()
    };
    if clamped != *geom {
        tracing::info!(
            saved_monitor = geom.monitor_name.as_deref().unwrap_or("<none>"),
            monitor = clamped.monitor_name.as_deref().unwrap_or("<none>"),
            width = clamped.width,
            height = clamped.height,
            "clamped the restored window into the current monitor layout"
        );
    }
    clamped
}

/// Clamp one rect into the layout, reporting the monitor it had to be moved
/// onto when it touched none of them.
///
/// `default_placement` stands in for a rect with no origin: such a record
/// cannot say where its window will be, so it is measured where the window will
/// open. The origin stays `None` either way — inventing one is the bug
/// [`geometry_from_bounds`] exists to avoid — and only the size can come back
/// changed.
fn clamp_saved_rect(
    rect: SavedRect,
    connected: &[MonitorWorkArea],
    default_placement: Bounds<Pixels>,
) -> (SavedRect, Option<&MonitorWorkArea>) {
    let bounds = logical_bounds(rect.x, rect.y, rect.width, rect.height, default_placement);
    let touched = connected
        .iter()
        .map(MonitorWorkArea::bounds)
        .filter(|area| area.intersects(&bounds))
        .reduce(|spanned, area| spanned.union(&area));
    let moved_to = if touched.is_some() { None } else { nearest_monitor(bounds, connected) };
    let Some(area) = touched.or_else(|| moved_to.map(MonitorWorkArea::bounds)) else {
        return (rect, None);
    };
    let fitted = fit_into(bounds, area);
    (
        SavedRect {
            x: rect.x.map(|_| logical_px_to_i32(f32::from(fitted.origin.x))),
            y: rect.y.map(|_| logical_px_to_i32(f32::from(fitted.origin.y))),
            width: u32::from(round_positive_f32_to_u16(f32::from(fitted.size.width))),
            height: u32::from(round_positive_f32_to_u16(f32::from(fitted.size.height))),
        },
        moved_to,
    )
}

/// Shrink a rect to `area` and slide it fully inside, keeping its origin as
/// close to where the user left it as the area allows.
fn fit_into(rect: Bounds<Pixels>, area: Bounds<Pixels>) -> Bounds<Pixels> {
    let fitted = size(rect.size.width.min(area.size.width), rect.size.height.min(area.size.height));
    let last_origin = point(area.right() - fitted.width, area.bottom() - fitted.height);
    Bounds { origin: rect.origin.clamp(&area.origin, &last_origin), size: fitted }
}

/// The monitor whose work-area centre is closest to the rect's — the standard
/// "put it back on the nearest screen" answer for a rect that is off the layout
/// entirely, and the one that keeps a window from a disconnected monitor next
/// to where its neighbours are.
fn nearest_monitor(
    rect: Bounds<Pixels>,
    connected: &[MonitorWorkArea],
) -> Option<&MonitorWorkArea> {
    let center = rect.center();
    connected.iter().min_by(|a, b| center_gap(a, center).total_cmp(&center_gap(b, center)))
}

/// Squared distance between a work area's centre and `center`; squared because
/// only the ordering is ever read.
fn center_gap(area: &MonitorWorkArea, center: Point<Pixels>) -> f32 {
    let area_center = area.bounds().center();
    let dx = f32::from(area_center.x) - f32::from(center.x);
    let dy = f32::from(area_center.y) - f32::from(center.y);
    dx.mul_add(dx, dy * dy)
}

/// Returns `true` if `geom` is within the safe size range. Sizes outside the
/// range are rejected to keep a corrupt or hostile state file from leaving the
/// window unusable.
#[must_use]
pub fn geometry_size_is_sane(geom: &WindowGeometry) -> bool {
    geom.width >= MIN_WINDOW_EDGE
        && geom.height >= MIN_WINDOW_EDGE
        && geom.width <= MAX_WINDOW_EDGE
        && geom.height <= MAX_WINDOW_EDGE
}

/// First-launch geometry-compat normalization (clamp + titlebar inset).
///
/// Geometry saved by the OS-decorated old client points its outer frame at the
/// top of a WM-drawn titlebar; the new client draws its own titlebar *inside*
/// the client area, so restoring the raw geometry would shrink the visible
/// terminal by one titlebar height. This runs once per legacy window:
///
/// 1. Size is clamped into `[MIN_WINDOW_EDGE, MAX_WINDOW_EDGE]`.
/// 2. The window height grows by `CUSTOM_TITLEBAR_HEIGHT` so the terminal area
///    below the new titlebar matches the old client area (skipped for maximized
///    and fullscreen windows, whose size the compositor overrides on restore).
/// 3. `titlebar_normalized` is set so a re-save + reload is idempotent.
///
/// Already-normalized geometry is returned unchanged.
#[must_use]
pub fn normalize_legacy_geometry(geom: &WindowGeometry) -> WindowGeometry {
    if geom.titlebar_normalized {
        return geom.clone();
    }

    let width = geom.width.clamp(MIN_WINDOW_EDGE, MAX_WINDOW_EDGE);
    // Grow the height so the terminal area under the in-window titlebar keeps
    // its old size, then clamp to the accepted range. Maximized windows are
    // resized by the compositor on restore, so leave their stored size alone.
    let height = if geom.effective_state() == WindowState::Windowed {
        geom.height.saturating_add(CUSTOM_TITLEBAR_HEIGHT).clamp(MIN_WINDOW_EDGE, MAX_WINDOW_EDGE)
    } else {
        geom.height.clamp(MIN_WINDOW_EDGE, MAX_WINDOW_EDGE)
    };

    WindowGeometry { width, height, titlebar_normalized: true, ..geom.clone() }
}

/// Turn a live GPUI window's bounds into the geometry that gets persisted.
///
/// This is the GPUI half of the legacy winit `capture_window_geometry`. GPUI
/// bounds are already logical pixels, so no scale-factor division is needed —
/// unlike winit, which reports physical pixels.
///
/// `origin` is split out of the bounds because a window whose origin is not
/// exposed (Wayland) must yield `x`/`y` of `None` rather than the bogus
/// `(0, 0)` GPUI reports there — that fake origin is what a later X11 session
/// restored into the corner. The caller decides with
/// [`crate::monitor::window_origin_is_exposed`]; the `Option` is what keeps the
/// decision from being quietly dropped here.
///
/// The virtual desktop and the font zoom level are left at their neutral values
/// for the caller to fill in with [`WindowGeometry::on_desktop`] and
/// [`WindowGeometry::at_zoom`]; neither is part of the bounds conversion.
///
/// `previous` is the last record this window produced. It is what carries the
/// pre-maximize/pre-fullscreen rect: once the window is no longer windowed its
/// own bounds are the work area, so the rect to return to can only come from
/// the reading taken before the transition.
#[must_use]
pub fn geometry_from_bounds(
    origin: Option<Point<Pixels>>,
    size: Size<Pixels>,
    observed: ObservedWindowState,
    monitor_name: Option<String>,
    previous: Option<&WindowGeometry>,
) -> WindowGeometry {
    let restore_rect = if observed.state == WindowState::Windowed {
        None
    } else {
        previous.and_then(WindowGeometry::windowed_rect)
    };
    WindowGeometry {
        x: origin.map(|origin| logical_px_to_i32(f32::from(origin.x))),
        y: origin.map(|origin| logical_px_to_i32(f32::from(origin.y))),
        width: u32::from(round_positive_f32_to_u16(f32::from(size.width))),
        height: u32::from(round_positive_f32_to_u16(f32::from(size.height))),
        state: observed.state,
        restore_state: observed.restore_state,
        monitor_name,
        desktop: None,
        // Anything captured from a live GPUI window is already in the new
        // coordinate system: the custom titlebar is inside these bounds.
        titlebar_normalized: true,
        // Filled in by the caller with [`WindowGeometry::at_zoom`]; the zoom is
        // no more a function of the bounds than the virtual desktop is.
        zoom: 0,
        legacy_maximized: None,
        restore_rect,
    }
}

/// Turn persisted geometry into the [`WindowBounds`] a window is opened at.
///
/// This is the GPUI half of the legacy winit `apply_window_geometry`. GPUI has
/// no post-creation "move + resize + maximize" sequence to race against the
/// compositor: the restored geometry is handed to `open_window` up front, so a
/// maximized window is maximized from its first frame and the pane grids sized
/// from `fallback`-independent bounds are the ones the window actually gets.
///
/// `fallback` supplies the origin when the saved record has none (Wayland).
///
/// A minimized record opens in the state it would unminimize to — GPUI maps the
/// window inside `Window::new`, so there is no "map me iconified" to ask for
/// here — and the caller issues `Window::minimize_window` once it is up. For a
/// maximized or fullscreen record the bounds GPUI wants are the *restore* size,
/// which is `restore_rect` when the transition was observed.
#[must_use]
pub fn window_bounds_for(geom: &WindowGeometry, fallback: Bounds<Pixels>) -> WindowBounds {
    let bounds = logical_bounds(geom.x, geom.y, geom.width, geom.height, fallback);
    let restore = geom
        .restore_rect
        .map_or(bounds, |rect| logical_bounds(rect.x, rect.y, rect.width, rect.height, fallback));
    match geom.effective_state() {
        // `effective_state` never answers Minimized; folding it in here keeps
        // the match exhaustive without an unreachable arm.
        WindowState::Windowed | WindowState::Minimized => WindowBounds::Windowed(bounds),
        WindowState::Maximized => WindowBounds::Maximized(restore),
        WindowState::Fullscreen => WindowBounds::Fullscreen(restore),
    }
}

fn logical_bounds(
    x: Option<i32>,
    y: Option<i32>,
    width: u32,
    height: u32,
    fallback: Bounds<Pixels>,
) -> Bounds<Pixels> {
    Bounds {
        origin: match (x, y) {
            (Some(x), Some(y)) => point(px(i32_to_logical_px(x)), px(i32_to_logical_px(y))),
            _ => fallback.origin,
        },
        size: size(px(u32_to_logical_px(width)), px(u32_to_logical_px(height))),
    }
}

/// Round a logical-pixel coordinate to the signed integer the record stores.
///
/// Written without a float cast (the workspace denies the pedantic cast lints)
/// by rounding the magnitude through the shared `u16` helper and re-applying the
/// sign; coordinates beyond ±65535 logical pixels do not occur on real displays.
pub fn logical_px_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let magnitude = i32::from(round_positive_f32_to_u16(value.abs()));
    if value.is_sign_negative() { -magnitude } else { magnitude }
}

fn u32_to_logical_px(value: u32) -> f32 {
    f32::from(u16::try_from(value.min(MAX_WINDOW_EDGE)).unwrap_or(u16::MAX))
}

fn i32_to_logical_px(value: i32) -> f32 {
    let clamped = value.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    f32::from(i16::try_from(clamped).unwrap_or(0))
}

/// Per-window geometry persistence using one file per window.
///
/// Files are stored at `$XDG_STATE_HOME/scribe/windows/<window_id>.toml`. Each
/// file contains a single [`WindowGeometry`]. This avoids race conditions when
/// multiple window processes save geometry simultaneously.
pub struct WindowRegistry {
    dir: Option<PathBuf>,
}

impl Default for WindowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowRegistry {
    /// Load the registry, resolving the directory path once.
    #[must_use]
    pub fn new() -> Self {
        Self { dir: current_state_dir().map(|dir| dir.join("windows")) }
    }

    /// Load geometry for a specific window, falling back to the default on any
    /// read or parse error.
    #[must_use]
    pub fn load(&self, window_id: WindowId) -> WindowGeometry {
        self.load_saved(window_id).unwrap_or_default()
    }

    /// Load geometry for a specific window, or `None` when nothing usable was
    /// persisted for it.
    ///
    /// The distinction matters at startup: [`Self::load`]'s default is a size
    /// hint, not a restore, and opening a window at it would override the
    /// grid-derived startup size for every launch that never saved geometry.
    /// Only a real on-disk record should displace that.
    #[must_use]
    pub fn load_saved(&self, window_id: WindowId) -> Option<WindowGeometry> {
        let path = self.window_path(window_id)?;
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(geom) => Some(adopt_legacy_state(geom)),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "window state parse error");
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read window state");
                None
            }
        }
    }

    /// Save geometry for a specific window.
    ///
    /// # Errors
    /// Returns [`StateError`] when the state directory is unavailable or the
    /// file cannot be created, serialized, or written.
    pub fn save(&self, window_id: WindowId, geom: &WindowGeometry) -> Result<(), StateError> {
        let path = self.window_path(window_id).ok_or(StateError::NoStateDir)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(geom)?;
        std::fs::write(&path, content)?;
        tracing::debug!(path = %path.display(), %window_id, "window geometry saved");
        Ok(())
    }

    /// Remove the geometry file for a window (when it is permanently closed).
    pub fn remove(&self, window_id: WindowId) {
        let Some(path) = self.window_path(window_id) else { return };
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), "failed to remove window state: {e}");
        }
    }

    fn window_path(&self, window_id: WindowId) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{}.toml", window_id.to_full_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_geom(width: u32, height: u32, state: WindowState) -> WindowGeometry {
        WindowGeometry {
            x: Some(100),
            y: Some(200),
            width,
            height,
            state,
            monitor_name: Some("DP-1".to_owned()),
            titlebar_normalized: false,
            ..WindowGeometry::default()
        }
    }

    fn test_bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds { origin: gpui::point(px(x), px(y)), size: gpui::size(px(width), px(height)) }
    }

    /// A capture on a platform that reports the window origin (X11, macOS).
    fn capture(
        bounds: Bounds<Pixels>,
        observed: ObservedWindowState,
        monitor: Option<String>,
        previous: Option<&WindowGeometry>,
    ) -> WindowGeometry {
        geometry_from_bounds(Some(bounds.origin), bounds.size, observed, monitor, previous)
    }

    // @lat: [[test#Window geometry compat#Legacy geometry gains titlebar inset]]
    #[test]
    fn legacy_geometry_grows_by_titlebar_height() {
        let normalized = normalize_legacy_geometry(&legacy_geom(1200, 800, WindowState::Windowed));
        assert_eq!(normalized.width, 1200);
        assert_eq!(normalized.height, 800 + CUSTOM_TITLEBAR_HEIGHT);
        assert!(normalized.titlebar_normalized);
        // Position and monitor survive the normalization.
        assert_eq!(normalized.x, Some(100));
        assert_eq!(normalized.y, Some(200));
        assert_eq!(normalized.monitor_name.as_deref(), Some("DP-1"));
    }

    // @lat: [[test#Window geometry compat#Normalization is idempotent]]
    #[test]
    fn normalization_is_idempotent() {
        let once = normalize_legacy_geometry(&legacy_geom(1200, 800, WindowState::Windowed));
        let twice = normalize_legacy_geometry(&once);
        assert_eq!(once, twice);
    }

    // @lat: [[test#Window geometry compat#Maximized geometry keeps its size]]
    #[test]
    fn maximized_geometry_keeps_size() {
        let normalized =
            normalize_legacy_geometry(&legacy_geom(1920, 1080, WindowState::Maximized));
        assert_eq!(normalized.height, 1080);
        assert!(normalized.titlebar_normalized);
    }

    // @lat: [[test#Window geometry compat#Out-of-range legacy size is clamped]]
    #[test]
    fn out_of_range_size_is_clamped() {
        let huge = normalize_legacy_geometry(&legacy_geom(999_999, 30, WindowState::Windowed));
        assert_eq!(huge.width, MAX_WINDOW_EDGE);
        // 30 + 36 = 66, already >= MIN_WINDOW_EDGE, so no clamp needed there.
        assert_eq!(huge.height, 30 + CUSTOM_TITLEBAR_HEIGHT);
        assert!(geometry_size_is_sane(&huge));
    }

    // @lat: [[test#Window geometry compat#Default geometry is already normalized]]
    #[test]
    fn default_geometry_is_pre_normalized() {
        let def = WindowGeometry::default();
        assert!(def.titlebar_normalized);
        assert_eq!(normalize_legacy_geometry(&def), def);
    }

    // @lat: [[test#Window geometry compat#Legacy TOML lacks the normalized flag]]
    #[test]
    fn legacy_toml_deserializes_unnormalized() {
        // A file written by the old client has no `titlebar_normalized` key.
        let toml = "\
x = 10
y = 20
width = 1000
height = 700
maximized = false
";
        let geom = adopt_legacy_state(toml::from_str(toml).expect("parse legacy toml"));
        assert!(!geom.titlebar_normalized);
        let normalized = normalize_legacy_geometry(&geom);
        assert_eq!(normalized.height, 700 + CUSTOM_TITLEBAR_HEIGHT);
    }

    // @lat: [[test#Window geometry compat#Legacy maximized bool folds into the state]]
    #[test]
    fn legacy_maximized_bool_folds_into_the_state() {
        let toml = "\
x = 10
y = 20
width = 1920
height = 1080
maximized = true
titlebar_normalized = true
";
        let geom = adopt_legacy_state(toml::from_str(toml).expect("parse legacy toml"));
        assert_eq!(geom.state, WindowState::Maximized);
        // The fold survives normalization's `titlebar_normalized` short-circuit,
        // which is why it does not live inside it.
        assert_eq!(normalize_legacy_geometry(&geom).state, WindowState::Maximized);
        // And it is never written back out as a bool.
        let round_tripped = toml::to_string_pretty(&geom).expect("serialize");
        assert!(!round_tripped.contains("maximized ="), "{round_tripped}");
        assert!(round_tripped.contains("state = \"maximized\""), "{round_tripped}");

        let windowed = adopt_legacy_state(
            toml::from_str("width = 800\nheight = 600\nmaximized = false\n").expect("parse"),
        );
        assert_eq!(windowed.state, WindowState::Windowed);
    }

    // @lat: [[test#Window geometry compat#Minimizing a maximized window keeps it maximized]]
    #[test]
    fn minimizing_a_maximized_window_keeps_it_maximized() {
        // The three window-manager flags are independent: a minimized window
        // keeps its maximized bits, which GPUI's `is_maximized()` hides.
        let observed = ObservedWindowState::from_wm_state(true, WindowState::Maximized);
        assert_eq!(observed.state, WindowState::Minimized);
        assert_eq!(observed.restore_state, WindowState::Maximized);

        let geom = capture(
            test_bounds(0.0, 0.0, 1920.0, 1080.0),
            observed,
            None,
            Some(&WindowGeometry {
                state: WindowState::Maximized,
                restore_rect: Some(SavedRect {
                    x: Some(100),
                    y: Some(200),
                    width: 1200,
                    height: 800,
                }),
                ..WindowGeometry::default()
            }),
        );
        assert_eq!(geom.effective_state(), WindowState::Maximized);
        let WindowBounds::Maximized(restore) =
            window_bounds_for(&geom, test_bounds(0.0, 0.0, 10.0, 10.0))
        else {
            panic!("a minimized-from-maximized record must reopen maximized");
        };
        // GPUI takes the restore size here, which is the pre-maximize rect.
        assert_eq!(restore, test_bounds(100.0, 200.0, 1200.0, 800.0));

        // Minimized from plain windowed reopens windowed at its own rect.
        let plain = ObservedWindowState::from_wm_state(true, WindowState::Windowed);
        assert_eq!(plain.restore_state, WindowState::Windowed);
        // Minimizing out of fullscreen keeps the fullscreen the same way.
        let from_fullscreen = ObservedWindowState::from_wm_state(true, WindowState::Fullscreen);
        assert_eq!(from_fullscreen.restore_state, WindowState::Fullscreen);
    }

    // @lat: [[test#Window geometry compat#Fullscreen record reopens fullscreen]]
    #[test]
    fn fullscreen_record_reopens_fullscreen() {
        let geom = WindowGeometry {
            state: WindowState::Fullscreen,
            ..legacy_geom(2560, 1440, WindowState::Fullscreen)
        };
        assert!(matches!(
            window_bounds_for(&geom, test_bounds(0.0, 0.0, 800.0, 600.0)),
            WindowBounds::Fullscreen(_)
        ));
        // A hand-broken record that unminimizes into minimization is read as
        // windowed rather than looping.
        let broken = WindowGeometry {
            state: WindowState::Minimized,
            restore_state: WindowState::Minimized,
            ..WindowGeometry::default()
        };
        assert_eq!(broken.effective_state(), WindowState::Windowed);
    }

    // @lat: [[test#Window geometry compat#Pre-maximize rect survives the transition]]
    #[test]
    fn pre_maximize_rect_survives_the_transition() {
        let windowed = capture(
            test_bounds(120.0, 64.0, 1440.0, 900.0),
            ObservedWindowState::default(),
            None,
            None,
        );
        assert_eq!(windowed.restore_rect, None, "a windowed record is its own restore rect");

        let maximized = capture(
            test_bounds(0.0, 0.0, 1920.0, 1080.0),
            ObservedWindowState::from_wm_state(false, WindowState::Maximized),
            None,
            Some(&windowed),
        );
        assert_eq!(
            maximized.restore_rect,
            Some(SavedRect { x: Some(120), y: Some(64), width: 1440, height: 900 })
        );

        // A later capture that is still maximized carries the same rect rather
        // than overwriting it with the work area.
        let still = capture(
            test_bounds(0.0, 0.0, 1920.0, 1080.0),
            ObservedWindowState::from_wm_state(false, WindowState::Maximized),
            None,
            Some(&maximized),
        );
        assert_eq!(still.restore_rect, maximized.restore_rect);

        // Unmaximizing drops it again.
        let back = capture(
            test_bounds(120.0, 64.0, 1440.0, 900.0),
            ObservedWindowState::default(),
            None,
            Some(&still),
        );
        assert_eq!(back.restore_rect, None);
    }

    // @lat: [[test#Window geometry compat#Live bounds round-trip through a record]]
    #[test]
    fn live_bounds_round_trip_through_a_record() {
        let bounds = test_bounds(120.0, 64.0, 1440.0, 900.0);
        let geom = capture(bounds, ObservedWindowState::default(), Some("dp-1".to_owned()), None);
        assert_eq!((geom.x, geom.y), (Some(120), Some(64)));
        assert_eq!((geom.width, geom.height), (1440, 900));
        assert!(geom.titlebar_normalized, "a live capture is already in the new coordinate system");

        let fallback = test_bounds(0.0, 0.0, 1.0, 1.0);
        let WindowBounds::Windowed(restored) = window_bounds_for(&geom, fallback) else {
            panic!("a non-maximized record must reopen windowed");
        };
        assert_eq!(restored, bounds);
    }

    // @lat: [[test#Window geometry compat#An unexposed origin is never invented]]
    #[test]
    fn unexposed_origin_is_never_invented() {
        // Wayland reports (0, 0) for every window; the caller withholds it.
        let bounds = test_bounds(0.0, 0.0, 1440.0, 900.0);
        let geom = geometry_from_bounds(
            None,
            bounds.size,
            ObservedWindowState::default(),
            Some("wayland-0".to_owned()),
            None,
        );
        assert_eq!((geom.x, geom.y), (None, None), "an origin that was not observed is not stored");
        assert_eq!((geom.width, geom.height), (1440, 900), "the size is still real");
        assert_eq!(geom.restore_origin(), None, "there is no placement to re-assert");

        // The layout clamp cannot resurrect it: off X11 the connected list is
        // empty, which it reads as "unverifiable, keep the record".
        let gated = clamp_geometry_to_layout(&geom, &[]);
        assert_eq!((gated.x, gated.y), (None, None));
        // So a later X11 start opens at the default placement, not the corner.
        let fallback = test_bounds(300.0, 200.0, 10.0, 10.0);
        let WindowBounds::Windowed(restored) = window_bounds_for(&gated, fallback) else {
            panic!("a windowed record must reopen windowed");
        };
        assert_eq!(restored.origin, fallback.origin);
    }

    // @lat: [[test#Window geometry compat#Maximized record reopens maximized]]
    #[test]
    fn maximized_record_reopens_maximized() {
        let geom = legacy_geom(1920, 1080, WindowState::Maximized);
        let fallback = test_bounds(0.0, 0.0, 800.0, 600.0);
        assert!(matches!(window_bounds_for(&geom, fallback), WindowBounds::Maximized(_)));
    }

    // @lat: [[test#Window geometry compat#A maximized record still has an origin to re-assert]]
    #[test]
    fn maximized_record_still_has_an_origin_to_re_assert() {
        // A legacy maximized record has no pre-maximize rect, and its own
        // origin is the work area's — still the right monitor to aim at.
        let mut geom = legacy_geom(1920, 1080, WindowState::Maximized);
        geom.x = Some(2160);
        assert_eq!(geom.restore_origin(), Some((2160, 200)));
        // Once one has been captured it wins: it is on the same monitor and is
        // where the window returns to when the move unmaximizes it.
        geom.restore_rect =
            Some(SavedRect { x: Some(2260), y: Some(120), width: 1200, height: 800 });
        assert_eq!(geom.restore_origin(), Some((2260, 120)));
        // A windowed record answers with its own origin, as it always did.
        assert_eq!(
            legacy_geom(1200, 800, WindowState::Windowed).restore_origin(),
            Some((100, 200))
        );
        // Nothing captured (Wayland) stays nothing.
        assert_eq!(WindowGeometry::default().restore_origin(), None);
    }

    // @lat: [[test#Window geometry compat#Position-less record keeps the fallback origin]]
    #[test]
    fn position_less_record_keeps_the_fallback_origin() {
        let geom = WindowGeometry { x: None, y: None, ..WindowGeometry::default() };
        let fallback = test_bounds(300.0, 200.0, 10.0, 10.0);
        let WindowBounds::Windowed(bounds) = window_bounds_for(&geom, fallback) else {
            panic!("a non-maximized record must reopen windowed");
        };
        assert_eq!(bounds.origin, fallback.origin);
        assert_eq!(bounds.size.width, px(1200.0));
    }

    // @lat: [[test#Window geometry compat#Sanity range rejects extremes]]
    #[test]
    fn sanity_range_rejects_extremes() {
        let windowed = WindowState::Windowed;
        assert!(!geometry_size_is_sane(&legacy_geom(0, 0, windowed)));
        assert!(!geometry_size_is_sane(&legacy_geom(39, 800, windowed)));
        assert!(!geometry_size_is_sane(&legacy_geom(1200, 16385, windowed)));
        assert!(geometry_size_is_sane(&legacy_geom(40, 40, windowed)));
        assert!(geometry_size_is_sane(&legacy_geom(16384, 16384, windowed)));
    }

    // @lat: [[test#Window geometry compat#The virtual desktop round-trips]]
    #[test]
    fn virtual_desktop_round_trips() {
        // The capture leaves the field to the caller, who fills it from the
        // window manager's `_NET_WM_DESKTOP`.
        let captured = capture(
            test_bounds(120.0, 64.0, 1440.0, 900.0),
            ObservedWindowState::default(),
            None,
            None,
        );
        assert_eq!(captured.desktop, None);
        let geom = captured.on_desktop(Some(3));
        let round_trip = |record: &WindowGeometry| {
            let text = toml::to_string_pretty(record).expect("serialize");
            toml::from_str::<WindowGeometry>(&text).expect("round-trip").desktop
        };
        assert_eq!(round_trip(&geom), Some(3));

        // 0xFFFFFFFF is EWMH's "all desktops"; it is a desktop id like any
        // other here, so a sticky window comes back sticky.
        assert_eq!(round_trip(&geom.clone().on_desktop(Some(u32::MAX))), Some(u32::MAX));

        // The layout clamp only touches geometry.
        assert_eq!(
            clamp_geometry_to_layout(&geom, &[work_area("DP-1", 0, 0, 1920, 1080)]).desktop,
            Some(3)
        );

        // A record written before the field existed, and a window manager with
        // no virtual desktops, both answer "nothing to restore".
        let legacy: WindowGeometry =
            toml::from_str("width = 800\nheight = 600\n").expect("parse legacy toml");
        assert_eq!(legacy.desktop, None);
    }

    fn work_area(name: &str, x: i32, y: i32, width: u32, height: u32) -> MonitorWorkArea {
        MonitorWorkArea { name: name.to_owned(), x, y, width, height }
    }

    // @lat: [[test#Window geometry compat#The font zoom level round-trips]]
    #[test]
    fn zoom_level_round_trips() {
        let captured = capture(
            test_bounds(120.0, 64.0, 1440.0, 900.0),
            ObservedWindowState::from_wm_state(false, WindowState::Maximized),
            None,
            Some(&WindowGeometry {
                restore_rect: Some(SavedRect { x: Some(10), y: Some(20), width: 800, height: 600 }),
                state: WindowState::Maximized,
                ..WindowGeometry::default()
            }),
        );
        assert_eq!(captured.zoom, 0, "the bounds conversion leaves the level to the caller");

        // A bare key written after `restore_rect`'s table would be read as part
        // of it, so the round-trip is taken with that table present.
        let geom = captured.at_zoom(-3);
        let text = toml::to_string_pretty(&geom).expect("serialize");
        assert_eq!(toml::from_str::<WindowGeometry>(&text).expect("round-trip"), geom, "{text}");

        // The clamp only touches geometry, so the level survives a monitor
        // layout change with it.
        assert_eq!(
            clamp_geometry_to_layout(&geom, &[work_area("DP-1", 0, 0, 1920, 1080)]).zoom,
            -3
        );
        // And a record written before the field existed restores unzoomed.
        let legacy: WindowGeometry =
            toml::from_str("width = 800\nheight = 600\n").expect("parse legacy toml");
        assert_eq!(legacy.zoom, 0);
    }

    // @lat: [[test#Window geometry compat#A window off the layout is clamped back onto it]]
    #[test]
    fn window_off_the_layout_is_clamped_back_onto_it() {
        // One 1920x1080 monitor left, with a 27px panel across its top.
        let connected = vec![work_area("DP-2", 0, 27, 1920, 1053)];

        // The window was 3840 wide on a monitor that is now gone.
        let mut geom = legacy_geom(3840, 2160, WindowState::Windowed);
        geom.x = Some(3840);
        geom.y = Some(0);
        geom.monitor_name = Some("DP-4".to_owned());
        let clamped = clamp_geometry_to_layout(&geom, &connected);
        assert_eq!((clamped.x, clamped.y), (Some(0), Some(27)), "moved onto the remaining monitor");
        assert_eq!((clamped.width, clamped.height), (1920, 1053), "and shrunk to its work area");
        assert_eq!(
            clamped.monitor_name.as_deref(),
            Some("DP-2"),
            "the record names the monitor it was moved onto, so the landing check agrees"
        );

        // A maximized record's pre-maximize rect is clamped with it: that is
        // the rect the placement move aims at and the window returns to.
        geom.state = WindowState::Maximized;
        geom.restore_rect =
            Some(SavedRect { x: Some(4000), y: Some(200), width: 2400, height: 1400 });
        let maximized = clamp_geometry_to_layout(&geom, &connected);
        assert_eq!(
            maximized.restore_rect,
            Some(SavedRect { x: Some(0), y: Some(27), width: 1920, height: 1053 })
        );
        assert_eq!(maximized.state, WindowState::Maximized, "only the geometry is touched");
    }

    // @lat: [[test#Window geometry compat#A reachable window is left where it is]]
    #[test]
    fn reachable_window_is_left_where_it_is() {
        let connected =
            vec![work_area("DP-2", 0, 27, 1920, 1053), work_area("DP-4", 1920, 27, 1920, 1053)];

        // Fully on a monitor: untouched, nil-UUID identity and all.
        let mut geom = legacy_geom(1200, 800, WindowState::Windowed);
        geom.x = Some(100);
        geom.y = Some(200);
        geom.monitor_name = Some(NIL_MONITOR_ID.to_owned());
        assert_eq!(clamp_geometry_to_layout(&geom, &connected), geom);

        // Deliberately spanning both monitors: still untouched, because the
        // clamp target is the union of the work areas the rect touches.
        geom.width = 2400;
        geom.x = Some(1200);
        assert_eq!(clamp_geometry_to_layout(&geom, &connected), geom);

        // No enumeration available (macOS, pure Wayland): nothing to verify
        // against, so even an absurd rect survives.
        geom.x = Some(9000);
        assert_eq!(clamp_geometry_to_layout(&geom, &[]), geom);
    }
}
