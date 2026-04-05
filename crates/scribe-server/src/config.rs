use std::path::PathBuf;

use tracing::{info, warn};

use scribe_common::config::UpdateConfig;
use scribe_common::error::ScribeError;

/// Maximum allowed scrollback lines to prevent excessive memory use.
const MAX_SCROLLBACK_LINES: u32 = 100_000;

#[allow(clippy::struct_excessive_bools, reason = "config struct with independent boolean flags")]
pub struct ScribeConfig {
    pub workspace_roots: Vec<PathBuf>,
    pub scrollback_lines: u32,
    pub shell_integration_enabled: bool,
    pub hide_codex_hook_logs: bool,
    pub preserve_ai_scrollback: bool,
    pub update: UpdateConfig,
}

impl Default for ScribeConfig {
    fn default() -> Self {
        Self {
            workspace_roots: Vec::new(),
            scrollback_lines: 10_000,
            shell_integration_enabled: true,
            hide_codex_hook_logs: false,
            preserve_ai_scrollback: false,
            update: UpdateConfig::default(),
        }
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

    let shell_integration_enabled = full.terminal.shell_integration.enabled;
    let hide_codex_hook_logs = full.terminal.hide_codex_hook_logs;
    let preserve_ai_scrollback = full.terminal.preserve_ai_scrollback;
    let update = full.update;

    info!(
        roots = workspace_roots.len(),
        scrollback_lines, hide_codex_hook_logs, preserve_ai_scrollback, "server config loaded"
    );

    Ok(ScribeConfig {
        workspace_roots,
        scrollback_lines,
        shell_integration_enabled,
        hide_codex_hook_logs,
        preserve_ai_scrollback,
        update,
    })
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}
