//! Persistent window geometry state for the GPUI client.
//!
//! Stores window position, size, maximized state, and monitor name in
//! `$XDG_STATE_HOME/scribe/windows/<window_id>.toml` (one file per window to
//! avoid save races between window processes). Ported from the legacy winit
//! client's `window_state.rs`; the winit `capture`/`apply` glue is replaced by
//! GPUI-native bounds conversion (the shell bead) since GPUI owns the window.
//! `monitor_name` is the `RandR` connector name resolved by [`crate::monitor`]
//! (GPUI's X11 display uuid is a nil placeholder — [`NIL_MONITOR_ID`]), and
//! [`gate_position_on_monitor`] drops a saved position whose monitor is
//! unknown or no longer connected.
//!
//! It also adds the first-launch **geometry-compat normalization**
//! ([`normalize_legacy_geometry`]): geometry persisted by the OS-decorated old
//! client restores mis-inset under the new custom titlebar, so the first launch
//! after cutover clamps the size and applies a titlebar inset once, recording
//! `titlebar_normalized` so it never runs twice.

use std::path::PathBuf;

use gpui::{Bounds, Pixels, WindowBounds, point, px, size};
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
    pub maximized: bool,
    pub monitor_name: Option<String>,
    /// Whether the geometry-compat titlebar inset has already been applied.
    /// Absent in legacy files (`serde(default)` → `false`).
    #[serde(default)]
    pub titlebar_normalized: bool,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            width: 1200,
            height: 800,
            maximized: false,
            monitor_name: None,
            // A freshly-created default is already in the new coordinate system.
            titlebar_normalized: true,
        }
    }
}

/// The nil UUID string GPUI's X11 backend reports as its display "uuid".
///
/// v0.1.0 cutover clients persisted this verbatim as `monitor_name`, so every
/// pre-connector-name record on X11 carries it. Restore treats it as "monitor
/// identity unknown" — the position is kept, because dropping it would discard
/// every such window's placement on the first post-upgrade start (the
/// side-by-side-windows-became-stacked regression). The record self-heals to a
/// `RandR` connector name on the next geometry capture.
pub const NIL_MONITOR_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Decide whether a saved absolute position may be applied on restore.
///
/// The position is dropped only when the record names a real monitor identity
/// that is verifiably no longer connected. `None` and the nil-UUID placeholder
/// mean the identity is unknown — it cannot be disproven, so the position is
/// kept (matching pre-gate behavior for legacy records). An empty `connected`
/// list means the platform cannot enumerate monitors (macOS, pure Wayland) —
/// likewise unverifiable, so the position is kept.
#[must_use]
pub fn saved_monitor_allows_position(saved: Option<&str>, connected: &[String]) -> bool {
    match saved {
        None | Some(NIL_MONITOR_ID) => true,
        Some(name) => connected.is_empty() || connected.iter().any(|c| c == name),
    }
}

/// Drop the saved position when [`saved_monitor_allows_position`] rejects the
/// record's monitor, keeping size and maximized state. The restore path then
/// opens the window at its default placement — the legacy client's "saved
/// monitor not found, letting OS place window" behavior.
#[must_use]
pub fn gate_position_on_monitor(geom: &WindowGeometry, connected: &[String]) -> WindowGeometry {
    if saved_monitor_allows_position(geom.monitor_name.as_deref(), connected) {
        geom.clone()
    } else {
        tracing::info!(
            monitor = geom.monitor_name.as_deref().unwrap_or("<none>"),
            "saved monitor no longer connected; dropping the saved window position"
        );
        WindowGeometry { x: None, y: None, ..geom.clone() }
    }
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
///    windows, whose size the compositor overrides on restore).
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
    let height = if geom.maximized {
        geom.height.clamp(MIN_WINDOW_EDGE, MAX_WINDOW_EDGE)
    } else {
        geom.height.saturating_add(CUSTOM_TITLEBAR_HEIGHT).clamp(MIN_WINDOW_EDGE, MAX_WINDOW_EDGE)
    };

    WindowGeometry { width, height, titlebar_normalized: true, ..geom.clone() }
}

/// Turn a live GPUI window's bounds into the geometry that gets persisted.
///
/// This is the GPUI half of the legacy winit `capture_window_geometry`. GPUI
/// bounds are already logical pixels, so no scale-factor division is needed —
/// unlike winit, which reports physical pixels. A window whose origin is not
/// exposed (Wayland) yields `x`/`y` of `None` rather than a bogus `(0, 0)`,
/// which is what keeps a later X11 session from restoring into the corner.
#[must_use]
pub fn geometry_from_bounds(
    bounds: Bounds<Pixels>,
    maximized: bool,
    monitor_name: Option<String>,
) -> WindowGeometry {
    WindowGeometry {
        x: Some(logical_px_to_i32(f32::from(bounds.origin.x))),
        y: Some(logical_px_to_i32(f32::from(bounds.origin.y))),
        width: u32::from(round_positive_f32_to_u16(f32::from(bounds.size.width))),
        height: u32::from(round_positive_f32_to_u16(f32::from(bounds.size.height))),
        maximized,
        monitor_name,
        // Anything captured from a live GPUI window is already in the new
        // coordinate system: the custom titlebar is inside these bounds.
        titlebar_normalized: true,
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
#[must_use]
pub fn window_bounds_for(geom: &WindowGeometry, fallback: Bounds<Pixels>) -> WindowBounds {
    let bounds = Bounds {
        origin: match (geom.x, geom.y) {
            (Some(x), Some(y)) => point(px(i32_to_logical_px(x)), px(i32_to_logical_px(y))),
            _ => fallback.origin,
        },
        size: size(px(u32_to_logical_px(geom.width)), px(u32_to_logical_px(geom.height))),
    };
    if geom.maximized { WindowBounds::Maximized(bounds) } else { WindowBounds::Windowed(bounds) }
}

/// Round a logical-pixel coordinate to the signed integer the record stores.
///
/// Written without a float cast (the workspace denies the pedantic cast lints)
/// by rounding the magnitude through the shared `u16` helper and re-applying the
/// sign; coordinates beyond ±65535 logical pixels do not occur on real displays.
fn logical_px_to_i32(value: f32) -> i32 {
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
                Ok(geom) => Some(geom),
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

    fn legacy_geom(width: u32, height: u32, maximized: bool) -> WindowGeometry {
        WindowGeometry {
            x: Some(100),
            y: Some(200),
            width,
            height,
            maximized,
            monitor_name: Some("DP-1".to_owned()),
            titlebar_normalized: false,
        }
    }

    // @lat: [[test#Window geometry compat#Legacy geometry gains titlebar inset]]
    #[test]
    fn legacy_geometry_grows_by_titlebar_height() {
        let normalized = normalize_legacy_geometry(&legacy_geom(1200, 800, false));
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
        let once = normalize_legacy_geometry(&legacy_geom(1200, 800, false));
        let twice = normalize_legacy_geometry(&once);
        assert_eq!(once, twice);
    }

    // @lat: [[test#Window geometry compat#Maximized geometry keeps its size]]
    #[test]
    fn maximized_geometry_keeps_size() {
        let normalized = normalize_legacy_geometry(&legacy_geom(1920, 1080, true));
        assert_eq!(normalized.height, 1080);
        assert!(normalized.titlebar_normalized);
    }

    // @lat: [[test#Window geometry compat#Out-of-range legacy size is clamped]]
    #[test]
    fn out_of_range_size_is_clamped() {
        let huge = normalize_legacy_geometry(&legacy_geom(999_999, 30, false));
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
        let geom: WindowGeometry = toml::from_str(toml).expect("parse legacy toml");
        assert!(!geom.titlebar_normalized);
        let normalized = normalize_legacy_geometry(&geom);
        assert_eq!(normalized.height, 700 + CUSTOM_TITLEBAR_HEIGHT);
    }

    // @lat: [[test#Window geometry compat#Live bounds round-trip through a record]]
    #[test]
    fn live_bounds_round_trip_through_a_record() {
        let bounds = Bounds {
            origin: gpui::point(px(120.0), px(64.0)),
            size: gpui::size(px(1440.0), px(900.0)),
        };
        let geom = geometry_from_bounds(bounds, false, Some("dp-1".to_owned()));
        assert_eq!((geom.x, geom.y), (Some(120), Some(64)));
        assert_eq!((geom.width, geom.height), (1440, 900));
        assert!(geom.titlebar_normalized, "a live capture is already in the new coordinate system");

        let fallback =
            Bounds { origin: gpui::point(px(0.0), px(0.0)), size: gpui::size(px(1.0), px(1.0)) };
        let WindowBounds::Windowed(restored) = window_bounds_for(&geom, fallback) else {
            panic!("a non-maximized record must reopen windowed");
        };
        assert_eq!(restored, bounds);
    }

    // @lat: [[test#Window geometry compat#Maximized record reopens maximized]]
    #[test]
    fn maximized_record_reopens_maximized() {
        let geom = WindowGeometry { maximized: true, ..legacy_geom(1920, 1080, true) };
        let fallback = Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(800.0), px(600.0)),
        };
        assert!(matches!(window_bounds_for(&geom, fallback), WindowBounds::Maximized(_)));
    }

    // @lat: [[test#Window geometry compat#Position-less record keeps the fallback origin]]
    #[test]
    fn position_less_record_keeps_the_fallback_origin() {
        let geom = WindowGeometry { x: None, y: None, ..WindowGeometry::default() };
        let fallback = Bounds {
            origin: gpui::point(px(300.0), px(200.0)),
            size: gpui::size(px(10.0), px(10.0)),
        };
        let WindowBounds::Windowed(bounds) = window_bounds_for(&geom, fallback) else {
            panic!("a non-maximized record must reopen windowed");
        };
        assert_eq!(bounds.origin, fallback.origin);
        assert_eq!(bounds.size.width, px(1200.0));
    }

    // @lat: [[test#Window geometry compat#Sanity range rejects extremes]]
    #[test]
    fn sanity_range_rejects_extremes() {
        assert!(!geometry_size_is_sane(&legacy_geom(0, 0, false)));
        assert!(!geometry_size_is_sane(&legacy_geom(39, 800, false)));
        assert!(!geometry_size_is_sane(&legacy_geom(1200, 16385, false)));
        assert!(geometry_size_is_sane(&legacy_geom(40, 40, false)));
        assert!(geometry_size_is_sane(&legacy_geom(16384, 16384, false)));
    }

    // @lat: [[test#Window geometry compat#Unknown monitor identity keeps the saved position]]
    #[test]
    fn unknown_monitor_identity_keeps_position() {
        let connected = vec!["DP-2".to_owned(), "DP-4".to_owned()];
        // Legacy nil-UUID and absent identities are unverifiable → keep.
        assert!(saved_monitor_allows_position(Some(NIL_MONITOR_ID), &connected));
        assert!(saved_monitor_allows_position(None, &connected));
        // A real name still connected → keep; verifiably gone → drop.
        assert!(saved_monitor_allows_position(Some("DP-4"), &connected));
        assert!(!saved_monitor_allows_position(Some("DP-7"), &connected));
        // No enumeration available (macOS, pure Wayland) → keep.
        assert!(saved_monitor_allows_position(Some("DP-7"), &[]));

        // The gate strips only x/y, and only for a verifiably gone monitor.
        let mut geom = legacy_geom(1200, 800, true);
        geom.monitor_name = Some(NIL_MONITOR_ID.to_owned());
        assert_eq!(gate_position_on_monitor(&geom, &connected), geom);
        geom.monitor_name = Some("DP-7".to_owned());
        let gated = gate_position_on_monitor(&geom, &connected);
        assert_eq!((gated.x, gated.y), (None, None));
        assert_eq!((gated.width, gated.height, gated.maximized), (1200, 800, true));
    }
}
