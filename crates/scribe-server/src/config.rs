use std::path::PathBuf;

use serde::Deserialize;
use tracing::{info, warn};

use scribe_common::error::ScribeError;

/// Maximum allowed scrollback lines to prevent excessive memory use.
const MAX_SCROLLBACK_LINES: u32 = 100_000;

pub struct ScribeConfig {
    pub workspace_roots: Vec<PathBuf>,
    pub scrollback_lines: u32,
}

impl Default for ScribeConfig {
    fn default() -> Self {
        Self { workspace_roots: Vec::new(), scrollback_lines: 10_000 }
    }
}

/// Raw TOML top-level structure.
#[derive(Deserialize)]
struct RawConfig {
    workspaces: Option<WorkspacesConfig>,
    terminal: Option<TerminalConfig>,
}

#[derive(Deserialize)]
struct WorkspacesConfig {
    roots: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TerminalConfig {
    scrollback_lines: Option<u32>,
}

pub fn load_config() -> Result<ScribeConfig, ScribeError> {
    let Some(config_dir) = dirs::config_dir() else {
        info!("no config directory found, using defaults");
        return Ok(ScribeConfig::default());
    };

    let config_path = config_dir.join("scribe").join("config.toml");

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(?config_path, "no config file found, using defaults");
            return Ok(ScribeConfig::default());
        }
        Err(e) => {
            return Err(ScribeError::ConfigError {
                reason: format!("failed to read {}: {e}", config_path.display()),
            });
        }
    };

    info!(?config_path, "loading config");

    let raw: RawConfig = toml::from_str(&content)
        .map_err(|e| ScribeError::ConfigError { reason: format!("config parse error: {e}") })?;

    let workspace_roots: Vec<PathBuf> = raw
        .workspaces
        .and_then(|w| w.roots)
        .unwrap_or_default()
        .into_iter()
        .map(|s| expand_tilde(&s))
        .filter(|p| {
            if p.is_absolute() {
                true
            } else {
                warn!(?p, "ignoring non-absolute workspace root");
                false
            }
        })
        .collect();

    let raw_scrollback = raw.terminal.and_then(|t| t.scrollback_lines).unwrap_or(10_000);
    if raw_scrollback > MAX_SCROLLBACK_LINES {
        warn!(
            requested = raw_scrollback,
            max = MAX_SCROLLBACK_LINES,
            "scrollback_lines clamped to maximum"
        );
    }
    let scrollback_lines = raw_scrollback.min(MAX_SCROLLBACK_LINES);

    Ok(ScribeConfig { workspace_roots, scrollback_lines })
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}
