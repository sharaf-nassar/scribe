//! Persistent window geometry state for the GPUI client.
//!
//! Stores window position, size, maximized state, and monitor name in
//! `$XDG_STATE_HOME/scribe/windows/<window_id>.toml` (one file per window to
//! avoid save races between window processes). Ported from the legacy winit
//! client's `window_state.rs`; the winit `capture`/`apply` glue is replaced by
//! GPUI-native bounds conversion (the shell bead) since GPUI owns the window.
//!
//! It also adds the first-launch **geometry-compat normalization**
//! ([`normalize_legacy_geometry`]): geometry persisted by the OS-decorated old
//! client restores mis-inset under the new custom titlebar, so the first launch
//! after cutover clamps the size and applies a titlebar inset once, recording
//! `titlebar_normalized` so it never runs twice.

use std::path::PathBuf;

use scribe_common::app::current_state_dir;
use scribe_common::ids::WindowId;
use serde::{Deserialize, Serialize};

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
        let Some(path) = self.window_path(window_id) else {
            return WindowGeometry::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "window state parse error");
                WindowGeometry::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => WindowGeometry::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read window state");
                WindowGeometry::default()
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

    // @lat: [[test#Window geometry compat#Sanity range rejects extremes]]
    #[test]
    fn sanity_range_rejects_extremes() {
        assert!(!geometry_size_is_sane(&legacy_geom(0, 0, false)));
        assert!(!geometry_size_is_sane(&legacy_geom(39, 800, false)));
        assert!(!geometry_size_is_sane(&legacy_geom(1200, 16385, false)));
        assert!(geometry_size_is_sane(&legacy_geom(40, 40, false)));
        assert!(geometry_size_is_sane(&legacy_geom(16384, 16384, false)));
    }
}
