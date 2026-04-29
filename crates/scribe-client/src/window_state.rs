//! Persistent window geometry state.
//!
//! Stores window position, size, maximized state, and monitor name in
//! `$XDG_STATE_HOME/scribe/state.toml` (defaults to `~/.local/state/scribe/state.toml`).

use std::path::PathBuf;

use scribe_common::app::current_state_dir;
use serde::{Deserialize, Serialize};
use winit::dpi::PhysicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

// ---------------------------------------------------------------------------
// StateError
// ---------------------------------------------------------------------------

/// Errors that can occur during state persistence.
///
/// These are always handled gracefully (logged, never fatal).
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
/// when the user later runs on X11.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub monitor_name: Option<String>,
}

impl Default for WindowGeometry {
    fn default() -> Self {
        Self { x: None, y: None, width: 1200, height: 800, maximized: false, monitor_name: None }
    }
}

// ---------------------------------------------------------------------------
// WindowRegistry (multi-window)
// ---------------------------------------------------------------------------

/// Per-window geometry persistence using one file per window.
///
/// Files are stored at `$XDG_STATE_HOME/scribe/windows/<window_id>.toml`.
/// Each file contains a single `WindowGeometry`.  This avoids race conditions
/// when multiple window processes save geometry simultaneously.
pub struct WindowRegistry {
    dir: Option<PathBuf>,
}

impl WindowRegistry {
    /// Load the registry, resolving the directory path once.
    pub fn new() -> Self {
        Self { dir: current_state_dir().map(|dir| dir.join("windows")) }
    }

    /// Load geometry for a specific window.
    pub fn load(&self, window_id: scribe_common::ids::WindowId) -> WindowGeometry {
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
    pub fn save(
        &self,
        window_id: scribe_common::ids::WindowId,
        geom: &WindowGeometry,
    ) -> Result<(), StateError> {
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
    pub fn remove(&self, window_id: scribe_common::ids::WindowId) {
        let Some(path) = self.window_path(window_id) else { return };
        let result = std::fs::remove_file(&path);
        if let Err(e) = result {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), "failed to remove window state: {e}");
            }
        }
    }

    /// Migrate the legacy `state.toml` into a per-window file.
    ///
    /// Called on first startup when no window files exist yet. Creates a file
    /// for the given `window_id` using the legacy geometry, then removes the
    /// old `state.toml`.
    pub fn migrate_legacy(
        &self,
        window_id: scribe_common::ids::WindowId,
    ) -> Option<WindowGeometry> {
        let legacy_path = current_state_dir().map(|dir| dir.join("state.toml"))?;
        let content = std::fs::read_to_string(&legacy_path).ok()?;
        let geom: WindowGeometry = toml::from_str(&content).ok()?;

        if self.save(window_id, &geom).is_ok() {
            if let Err(e) = std::fs::remove_file(&legacy_path) {
                tracing::warn!(path = %legacy_path.display(), "failed to remove legacy state: {e}");
            }
            tracing::info!(%window_id, "migrated legacy state.toml to per-window file");
        }

        Some(geom)
    }

    fn window_path(&self, window_id: scribe_common::ids::WindowId) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{}.toml", window_id.to_full_string())))
    }
}

// ---------------------------------------------------------------------------
// Capture / Apply
// ---------------------------------------------------------------------------

/// Capture the current window geometry.
///
/// On Wayland, `outer_position()` is unavailable — position is stored as
/// `None` rather than a misleading `(0, 0)`.
pub fn capture_window_geometry(window: &Window) -> WindowGeometry {
    let scale = window.scale_factor();
    let size = window.outer_size().to_logical::<u32>(scale);
    let pos = window.outer_position().ok();
    let monitor_name = window.current_monitor().and_then(|m| m.name());

    WindowGeometry {
        x: pos.map(|p| {
            let lp = p.to_logical::<i32>(scale);
            lp.x
        }),
        y: pos.map(|p| {
            let lp = p.to_logical::<i32>(scale);
            lp.y
        }),
        width: size.width,
        height: size.height,
        maximized: window.is_maximized(),
        monitor_name,
    }
}

/// Returns `true` if `geom` is within the safe size range and apply will
/// actually mutate the window.  Sizes outside the range are rejected to
/// keep a corrupt or hostile state file from leaving the window unusable.
pub fn geometry_size_is_sane(geom: &WindowGeometry) -> bool {
    geom.width >= 40 && geom.height >= 40 && geom.width <= 16384 && geom.height <= 16384
}

/// Compute the eventual physical inner size implied by saved geometry.
///
/// Cold-restart replay needs to know the size the window will settle on
/// before the compositor has acknowledged `request_inner_size` and
/// `set_maximized(true)` — both are async on most compositors and may not
/// be reflected by `window.inner_size()` until a later configure event.
/// The saved width and height are logical pixels, so converting through
/// winit's [`winit::dpi::LogicalSize::to_physical`] with the current scale
/// factor yields the equivalent physical inner size.  The result is
/// clamped to at least 1×1 so callers can always treat it as a non-zero
/// viewport.
pub fn expected_physical_size(geom: &WindowGeometry, scale_factor: f32) -> PhysicalSize<u32> {
    let logical: winit::dpi::LogicalSize<u32> =
        winit::dpi::LogicalSize::new(geom.width, geom.height);
    let physical: PhysicalSize<u32> = logical.to_physical(f64::from(scale_factor));
    PhysicalSize::new(physical.width.max(1), physical.height.max(1))
}

/// Apply saved geometry to a window.
///
/// Restores position only if the saved monitor is still connected and
/// position was captured. Always restores size. Sets maximized state last.
/// Returns `false` (without mutating the window) when `geom` fails
/// [`geometry_size_is_sane`]; callers can then fall back to the OS default
/// instead of trusting the rejected geometry as the eventual viewport.
pub fn apply_window_geometry(
    event_loop: &ActiveEventLoop,
    window: &Window,
    geom: &WindowGeometry,
) -> bool {
    if !geometry_size_is_sane(geom) {
        tracing::warn!(
            width = geom.width,
            height = geom.height,
            "saved window size out of range, skipping restore"
        );
        return false;
    }

    // Restore position only if we have coordinates and the saved monitor is
    // still connected.
    if let (Some(x), Some(y)) = (geom.x, geom.y) {
        let monitor_found = geom.monitor_name.as_ref().is_some_and(|saved_name| {
            event_loop
                .available_monitors()
                .any(|m| m.name().as_deref() == Some(saved_name.as_str()))
        });

        if monitor_found {
            window.set_outer_position(winit::dpi::LogicalPosition::new(f64::from(x), f64::from(y)));
            tracing::debug!(x, y, "restored window position");
        } else if geom.monitor_name.is_some() {
            tracing::info!(
                monitor = ?geom.monitor_name,
                "saved monitor not found, letting OS place window"
            );
        }
    }

    // Always set the initial size from saved geometry.  For maximized
    // windows this provides a sensible pre-configure buffer so the GPU
    // surface and pane grids start at a reasonable size while we wait for
    // the compositor to acknowledge the maximized state.  Without this,
    // Wayland compositors may leave inner_size() at a tiny default until
    // the first configure event arrives, causing shells to start with the
    // wrong terminal dimensions.
    let _applied = window.request_inner_size(winit::dpi::LogicalSize::new(
        f64::from(geom.width),
        f64::from(geom.height),
    ));
    tracing::debug!(width = geom.width, height = geom.height, "restored window size");

    if geom.maximized {
        window.set_maximized(true);
        tracing::debug!("restored maximized state");
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maximized_geom(width: u32, height: u32) -> WindowGeometry {
        WindowGeometry { x: None, y: None, width, height, maximized: true, monitor_name: None }
    }

    #[test]
    fn expected_physical_size_at_unit_scale_matches_logical() {
        let geom = maximized_geom(1920, 1080);
        assert_eq!(expected_physical_size(&geom, 1.0), PhysicalSize::new(1920, 1080));
    }

    #[test]
    fn expected_physical_size_scales_with_dpi() {
        let geom = maximized_geom(1280, 720);
        assert_eq!(expected_physical_size(&geom, 2.0), PhysicalSize::new(2560, 1440));
    }

    #[test]
    fn expected_physical_size_handles_fractional_scale() {
        let geom = maximized_geom(1366, 768);
        let size = expected_physical_size(&geom, 1.5);
        // 1366 * 1.5 = 2049, 768 * 1.5 = 1152.
        assert_eq!(size, PhysicalSize::new(2049, 1152));
    }

    #[test]
    fn expected_physical_size_clamps_to_at_least_one_pixel() {
        // A tiny-but-valid scale factor multiplied with the smallest
        // accepted geometry rounds down to 0×0 inside winit's
        // `to_physical`; the clamp keeps the result usable as a viewport.
        let geom = maximized_geom(40, 40);
        let size = expected_physical_size(&geom, 0.01);
        assert!(size.width >= 1 && size.height >= 1);
    }

    #[test]
    fn geometry_size_is_sane_rejects_extremes() {
        assert!(!geometry_size_is_sane(&maximized_geom(0, 0)));
        assert!(!geometry_size_is_sane(&maximized_geom(39, 800)));
        assert!(!geometry_size_is_sane(&maximized_geom(1200, 16385)));
        assert!(geometry_size_is_sane(&maximized_geom(40, 40)));
        assert!(geometry_size_is_sane(&maximized_geom(1920, 1080)));
        assert!(geometry_size_is_sane(&maximized_geom(16384, 16384)));
    }
}
