//! Settings window state persistence.
//!
//! Reads/writes `~/.local/state/scribe/settings_state.toml` containing the
//! window geometry and an `open` flag for restart restoration.

use std::path::PathBuf;

use scribe_common::app::current_state_dir;

use crate::SettingsWindowGeometry;

/// Persisted settings window state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SettingsState {
    /// Whether the settings window was open when the process last exited.
    #[serde(default)]
    pub open: bool,
    /// Saved window geometry (position + size).
    pub geometry: Option<SettingsWindowGeometry>,
}

/// Resolve the state file path: `$XDG_STATE_HOME/scribe/settings_state.toml`.
fn state_path() -> Option<PathBuf> {
    current_state_dir().map(|dir| dir.join("settings_state.toml"))
}

/// Load settings state from disk. Returns defaults if absent or unparseable.
pub fn load() -> SettingsState {
    let Some(path) = state_path() else {
        return SettingsState::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "settings state parse error, using defaults");
            SettingsState::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SettingsState::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "failed to read settings state");
            SettingsState::default()
        }
    }
}

/// Save settings state to disk. Creates parent directories if needed.
pub fn save(state: &SettingsState) {
    let Some(path) = state_path() else {
        tracing::warn!("no XDG state directory, cannot save settings state");
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
                tracing::warn!(path = %path.display(), "failed to write settings state: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize settings state: {e}"),
    }
}
