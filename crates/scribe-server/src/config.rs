use std::path::PathBuf;

use tracing::{info, warn};

use scribe_common::config::{ClipboardPolicyConfig, RemoteConfig, UpdateConfig};
use scribe_common::error::ScribeError;

/// Maximum allowed scrollback lines to prevent excessive memory use.
const MAX_SCROLLBACK_LINES: u32 = 100_000;

pub struct AiTerminalConfig {
    pub preserve_ai_scrollback: bool,
}

impl Default for AiTerminalConfig {
    fn default() -> Self {
        Self { preserve_ai_scrollback: true }
    }
}

pub struct ScribeConfig {
    pub workspace_roots: Vec<PathBuf>,
    pub scrollback_lines: u32,
    pub shell_integration_enabled: bool,
    pub ai_terminal: AiTerminalConfig,
    pub update: UpdateConfig,
    /// Spec 010 T036: OSC 52 clipboard policy snapshot exposed on the
    /// server-local config so [`crate::ipc_server::handle_config_reloaded`]
    /// can fan it out to every PTY reader's
    /// [`crate::clipboard_state::ClipboardBurstState`] on each reload.
    pub clipboard_policy: ClipboardPolicyConfig,
    /// Feature 013: opt-in Tailscale remote-control listener config. Threaded
    /// through here so the config-reload path can start, stop, or rebind the
    /// remote listener live — never a server restart.
    pub remote: RemoteConfig,
}

impl Default for ScribeConfig {
    fn default() -> Self {
        Self {
            workspace_roots: Vec::new(),
            scrollback_lines: 10_000,
            shell_integration_enabled: true,
            ai_terminal: AiTerminalConfig::default(),
            update: UpdateConfig::default(),
            clipboard_policy: ClipboardPolicyConfig::default(),
            remote: RemoteConfig::default(),
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

    let shell_integration_enabled = full.terminal.ai_session.shell_integration.enabled;
    let ai_terminal = AiTerminalConfig {
        preserve_ai_scrollback: full.terminal.ai_session.preserve_ai_scrollback,
    };
    let update = full.update;
    let clipboard_policy = full.terminal.clipboard_policy;
    let remote = full.remote;

    info!(
        roots = workspace_roots.len(),
        scrollback_lines,
        preserve_ai_scrollback = ai_terminal.preserve_ai_scrollback,
        clipboard_read_mode = ?clipboard_policy.read_mode,
        clipboard_write_mode = ?clipboard_policy.write_mode,
        "server config loaded"
    );

    Ok(ScribeConfig {
        workspace_roots,
        scrollback_lines,
        shell_integration_enabled,
        ai_terminal,
        update,
        clipboard_policy,
        remote,
    })
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}
