//! Persistent window geometry state.
//!
//! Stores window position, size, maximized state, and monitor name in
//! `$XDG_STATE_HOME/scribe/state.toml` (defaults to `~/.local/state/scribe/state.toml`).
//! Uses a generic `StateStore<T>` that can be reused for other client-side
//! runtime state in the future.

use std::marker::PhantomData;
use std::path::PathBuf;

use scribe_common::app::current_state_dir;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
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

// ---------------------------------------------------------------------------
// StateStore<T>
// ---------------------------------------------------------------------------

/// Generic persistent state store backed by a TOML file.
///
/// Resolves `$XDG_STATE_HOME/scribe/<filename>` once at construction and
/// provides `load`/`save` with graceful degradation on errors.
#[allow(dead_code, reason = "generic state store retained for future client-side state")]
pub struct StateStore<T> {
    path: Option<PathBuf>,
    _marker: PhantomData<T>,
}

#[allow(dead_code, reason = "generic state store retained for future client-side state")]
impl<T: Serialize + DeserializeOwned + Default> StateStore<T> {
    /// Load state from `$XDG_STATE_HOME/scribe/<filename>`.
    ///
    /// Returns a store with `Default::default()` data if the file is absent,
    /// unreadable, or unparseable.
    pub fn load(filename: &str) -> (Self, T) {
        let path = current_state_dir().map(|dir| dir.join(filename));

        let data = path.as_ref().map_or_else(
            || {
                tracing::info!("no XDG state directory found, using defaults");
                T::default()
            },
            |p| match std::fs::read_to_string(p) {
                Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                    tracing::warn!(path = %p.display(), error = %e, "state file parse error, using defaults");
                    T::default()
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::info!(path = %p.display(), "no state file found, using defaults");
                    T::default()
                }
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "failed to read state file, using defaults");
                    T::default()
                }
            },
        );

        (Self { path, _marker: PhantomData }, data)
    }

    /// Write state to the backing file.
    ///
    /// Creates parent directories if they do not exist.
    pub fn save(&self, data: &T) -> Result<(), StateError> {
        let path = self.path.as_ref().ok_or(StateError::NoStateDir)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(data)?;
        std::fs::write(path, content)?;

        tracing::debug!(path = %path.display(), "window state saved");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WindowGeometry
// ---------------------------------------------------------------------------

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

    /// List all saved window IDs (for startup restoration).
    #[allow(dead_code, reason = "public API for future multi-window startup restoration")]
    pub fn saved_window_ids(&self) -> Vec<scribe_common::ids::WindowId> {
        let Some(dir) = &self.dir else { return Vec::new() };
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let stem = std::path::Path::new(&name).file_stem()?.to_str()?;
                stem.parse::<scribe_common::ids::WindowId>().ok()
            })
            .collect()
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
    let size = window.outer_size();
    let pos = window.outer_position().ok();
    let monitor_name = window.current_monitor().and_then(|m| m.name());

    WindowGeometry {
        x: pos.map(|p| p.x),
        y: pos.map(|p| p.y),
        width: size.width,
        height: size.height,
        maximized: window.is_maximized(),
        monitor_name,
    }
}

/// Apply saved geometry to a window.
///
/// Restores position only if the saved monitor is still connected and
/// position was captured. Always restores size. Sets maximized state last.
pub fn apply_window_geometry(event_loop: &ActiveEventLoop, window: &Window, geom: &WindowGeometry) {
    // Validate size is sane before applying.
    if geom.width < 40 || geom.height < 40 || geom.width > 16384 || geom.height > 16384 {
        tracing::warn!(
            width = geom.width,
            height = geom.height,
            "saved window size out of range, skipping restore"
        );
        return;
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
            window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            tracing::debug!(x, y, "restored window position");
        } else if geom.monitor_name.is_some() {
            tracing::info!(
                monitor = ?geom.monitor_name,
                "saved monitor not found, letting OS place window"
            );
        }
    }

    // Always restore size (unless maximized — the WM handles that).
    if !geom.maximized {
        // request_inner_size returns an optional immediate size; we don't need
        // it since handle_resize will fire from the resulting Resized event.
        let _applied =
            window.request_inner_size(winit::dpi::PhysicalSize::new(geom.width, geom.height));
        tracing::debug!(width = geom.width, height = geom.height, "restored window size");
    }

    if geom.maximized {
        window.set_maximized(true);
        tracing::debug!("restored maximized state");
    }
}
