//! Driver window state persistence.
//!
//! Reads/writes `$XDG_STATE_HOME/scribe/driver_state.toml` containing the
//! window geometry and an `open` flag for restart restoration.

use std::path::PathBuf;

use crate::DriverWindowGeometry;

/// Persisted driver window state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DriverState {
    /// Whether the driver window was open when the process last exited.
    #[serde(default)]
    pub open: bool,
    /// Saved window geometry (position + size).
    pub geometry: Option<DriverWindowGeometry>,
}

/// Resolve the state file path: `$XDG_STATE_HOME/scribe/driver_state.toml`.
fn state_path() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("scribe").join("driver_state.toml"))
}

/// Load driver state from disk. Returns defaults if absent or unparseable.
pub fn load() -> DriverState {
    let Some(path) = state_path() else {
        return DriverState::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "driver state parse error, using defaults"
            );
            DriverState::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DriverState::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to read driver state");
            DriverState::default()
        }
    }
}

/// Save driver state to disk. Creates parent directories if needed.
pub fn save(state: &DriverState) {
    let Some(path) = state_path() else {
        tracing::warn!("no XDG state directory, cannot save driver state");
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(path = %parent.display(), "failed to create state dir: {e}");
            return;
        }
    }
    match toml::to_string_pretty(state) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&path, content) {
                tracing::warn!(path = %path.display(), "failed to write driver state: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize driver state: {e}"),
    }
}
