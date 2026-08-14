use std::path::PathBuf;

use tracing::{info, warn};

use scribe_common::config::{ClipboardPolicyConfig, GithubCiConfig, RemoteConfig, UpdateConfig};
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
    /// Features 013/014: opt-in remote-control listener config. Carries both
    /// the Tailscale `[remote]` listener (013) and the nested `[remote.lan]`
    /// sub-config (014, reachable at `remote.lan`). Threaded through here so the
    /// config-reload path can start, stop, or rebind each transport's listener
    /// live — never a server restart.
    pub remote: RemoteConfig,
    /// Spec 020: `terminal.images.enabled`, the default-on terminal-graphics
    /// master switch. Mirrored into the process-wide switch by the startup and
    /// reload paths, which own the transition and the resource release it
    /// implies.
    pub images_enabled: bool,
    /// Whether a qualifying local push may start GitHub CI tracking.
    pub github_ci: GithubCiConfig,
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
            images_enabled: true,
            github_ci: GithubCiConfig::default(),
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
    let images_enabled = full.terminal.images.enabled;
    let github_ci = full.github_ci;

    info!(
        roots = workspace_roots.len(),
        scrollback_lines,
        preserve_ai_scrollback = ai_terminal.preserve_ai_scrollback,
        clipboard_read_mode = ?clipboard_policy.read_mode,
        clipboard_write_mode = ?clipboard_policy.write_mode,
        images_enabled,
        github_ci_enabled = github_ci.enabled,
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
        images_enabled,
        github_ci,
    })
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}
