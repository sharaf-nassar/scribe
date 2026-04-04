use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app::current_config_dir;
use crate::config::{ScribeConfig, load_config, save_config};
use crate::error::ScribeError;

const DEFAULT_PROFILE_NAME: &str = "default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileStore {
    #[serde(default = "default_profile_name")]
    pub active_profile: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, ScribeConfig>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self { active_profile: default_profile_name(), profiles: BTreeMap::new() }
    }
}

fn default_profile_name() -> String {
    DEFAULT_PROFILE_NAME.to_owned()
}

fn validate_profile_name(name: &str) -> Result<String, ScribeError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ScribeError::ConfigError {
            reason: String::from("profile name must not be empty"),
        });
    }

    if trimmed.chars().any(char::is_control) {
        return Err(ScribeError::ConfigError {
            reason: String::from("profile name must not contain control characters"),
        });
    }

    Ok(trimmed.to_owned())
}

fn ensure_profiles_dir() -> Result<PathBuf, ScribeError> {
    let Some(config_dir) = current_config_dir() else {
        return Err(ScribeError::ConfigError {
            reason: String::from("could not determine config directory"),
        });
    };

    std::fs::create_dir_all(&config_dir).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to create config directory {}: {e}", config_dir.display()),
    })?;
    Ok(config_dir)
}

#[must_use]
pub fn profile_store_path() -> Option<PathBuf> {
    current_config_dir().map(|dir| dir.join("profiles.toml"))
}

fn bootstrap_store_from_current_config() -> Result<ProfileStore, ScribeError> {
    let mut store = ProfileStore::default();
    store.profiles.insert(default_profile_name(), load_config()?);
    Ok(store)
}

fn normalize_store(store: &mut ProfileStore) -> Result<(), ScribeError> {
    if store.profiles.is_empty() {
        *store = bootstrap_store_from_current_config()?;
        return Ok(());
    }

    if store.active_profile.trim().is_empty() {
        store.active_profile = default_profile_name();
    }

    if !store.profiles.contains_key(&store.active_profile) {
        if let Some(first) = store.profiles.keys().next().cloned() {
            store.active_profile = first;
        }
    }

    Ok(())
}

pub fn load_profile_store() -> Result<ProfileStore, ScribeError> {
    let Some(path) = profile_store_path() else {
        return Err(ScribeError::ConfigError {
            reason: String::from("could not determine config directory"),
        });
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return bootstrap_store_from_current_config();
        }
        Err(e) => {
            return Err(ScribeError::ConfigError {
                reason: format!("failed to read {}: {e}", path.display()),
            });
        }
    };

    let mut store: ProfileStore = toml::from_str(&content).map_err(|e| {
        ScribeError::ConfigError { reason: format!("profile store parse error: {e}") }
    })?;
    normalize_store(&mut store)?;
    Ok(store)
}

pub fn save_profile_store(store: &ProfileStore) -> Result<(), ScribeError> {
    let mut normalized = store.clone();
    normalize_store(&mut normalized)?;

    let scribe_dir = ensure_profiles_dir()?;
    let path = scribe_dir.join("profiles.toml");
    let content = toml::to_string_pretty(&normalized).map_err(|e| ScribeError::ConfigError {
        reason: format!("profile store serialize error: {e}"),
    })?;
    std::fs::write(&path, content).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to write {}: {e}", path.display()),
    })?;
    Ok(())
}

pub fn list_profiles() -> Result<Vec<String>, ScribeError> {
    let store = load_profile_store()?;
    Ok(store.profiles.into_keys().collect())
}

pub fn active_profile_name() -> Result<String, ScribeError> {
    Ok(load_profile_store()?.active_profile)
}

pub fn save_current_as_profile(name: &str) -> Result<String, ScribeError> {
    let profile_name = validate_profile_name(name)?;
    let mut store = load_profile_store()?;
    store.profiles.insert(profile_name.clone(), load_config()?);
    save_profile_store(&store)?;
    Ok(profile_name)
}

pub fn switch_profile(name: &str) -> Result<ScribeConfig, ScribeError> {
    let profile_name = validate_profile_name(name)?;
    let mut store = load_profile_store()?;
    let config = store.profiles.get(&profile_name).cloned().ok_or_else(|| {
        ScribeError::ConfigError { reason: format!("profile not found: {profile_name}") }
    })?;
    save_config(&config)?;
    store.active_profile = profile_name;
    save_profile_store(&store)?;
    Ok(config)
}

pub fn export_profile(name: &str, path: &Path) -> Result<PathBuf, ScribeError> {
    let profile_name = validate_profile_name(name)?;
    let store = load_profile_store()?;
    let config = store.profiles.get(&profile_name).ok_or_else(|| ScribeError::ConfigError {
        reason: format!("profile not found: {profile_name}"),
    })?;
    let content = toml::to_string_pretty(config).map_err(|e| ScribeError::ConfigError {
        reason: format!("profile serialize error: {e}"),
    })?;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| ScribeError::ConfigError {
            reason: format!("failed to create export directory {}: {e}", parent.display()),
        })?;
    }
    std::fs::write(path, content).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to write {}: {e}", path.display()),
    })?;
    Ok(path.to_path_buf())
}

pub fn import_profile(name: &str, path: &Path, activate: bool) -> Result<String, ScribeError> {
    let profile_name = validate_profile_name(name)?;
    let content = std::fs::read_to_string(path).map_err(|e| ScribeError::ConfigError {
        reason: format!("failed to read {}: {e}", path.display()),
    })?;
    let mut config: ScribeConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("profile parse error: {e}") })?;
    config.appearance = config.appearance.clamped();

    let mut store = load_profile_store()?;
    store.profiles.insert(profile_name.clone(), config.clone());
    if activate {
        save_config(&config)?;
        store.active_profile.clone_from(&profile_name);
    }
    save_profile_store(&store)?;
    Ok(profile_name)
}
