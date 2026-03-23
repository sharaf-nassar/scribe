use std::path::PathBuf;

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

pub fn load_config() -> Result<ScribeConfig, ScribeError> {
    let full = scribe_common::config::load_config()?;

    let workspace_roots: Vec<PathBuf> = full
        .workspaces
        .roots
        .iter()
        .map(|s| expand_tilde(s))
        .filter(|p| {
            if p.is_absolute() {
                true
            } else {
                warn!(?p, "ignoring non-absolute workspace root");
                false
            }
        })
        .collect();

    let raw_scrollback = full.terminal.scrollback_lines;
    if raw_scrollback > MAX_SCROLLBACK_LINES {
        warn!(
            requested = raw_scrollback,
            max = MAX_SCROLLBACK_LINES,
            "scrollback_lines clamped to maximum"
        );
    }
    let scrollback_lines = raw_scrollback.min(MAX_SCROLLBACK_LINES);

    info!(roots = workspace_roots.len(), scrollback_lines, "server config loaded");

    Ok(ScribeConfig { workspace_roots, scrollback_lines })
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}
