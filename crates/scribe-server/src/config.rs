use std::path::PathBuf;

use tracing::{info, warn};

use scribe_common::config::{
    AgentApiConfig, ClipboardPolicyConfig, GithubCiConfig, RemoteConfig, UpdateConfig,
};
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
    /// Live policy and bounds for the local agent control surface.
    pub agent_api: AgentApiConfig,
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
            agent_api: AgentApiConfig::default(),
        }
    }
}

pub fn load_config() -> Result<ScribeConfig, ScribeError> {
    Ok(project_config(scribe_common::config::load_config()?))
}

/// Project the shared config into the server-owned snapshot.
///
/// `load_config` is called at startup and on every `ConfigReloaded`, so this
/// projection keeps the agent policy and limits live without a restart.
fn project_config(full: scribe_common::config::ScribeConfig) -> ScribeConfig {
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
    let agent_api = full.agent_api;

    let config = ScribeConfig {
        workspace_roots,
        scrollback_lines,
        shell_integration_enabled,
        ai_terminal,
        update,
        clipboard_policy,
        remote,
        images_enabled,
        github_ci,
        agent_api,
    };

    info!(
        roots = config.workspace_roots.len(),
        scrollback_lines = config.scrollback_lines,
        preserve_ai_scrollback = config.ai_terminal.preserve_ai_scrollback,
        clipboard_read_mode = ?config.clipboard_policy.read_mode,
        clipboard_write_mode = ?config.clipboard_policy.write_mode,
        images_enabled = config.images_enabled,
        github_ci_enabled = config.github_ci.enabled,
        agent_read_metadata = ?config.agent_api.read_metadata,
        agent_read_content = ?config.agent_api.read_content,
        "server config loaded"
    );

    config
}

fn expand_tilde(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
    )
}

#[cfg(test)]
mod tests {
    use super::project_config;
    use scribe_common::agent::AgentPolicyMode;

    #[test]
    fn projects_agent_api_config_for_each_reload() {
        let first: scribe_common::config::ScribeConfig =
            toml::from_str("[agent_api]\nread_metadata = \"allow\"\n")
                .expect("first config should parse");
        let second: scribe_common::config::ScribeConfig =
            toml::from_str("[agent_api]\nread_metadata = \"prompt\"\n")
                .expect("second config should parse");

        assert_eq!(project_config(first).agent_api.read_metadata, AgentPolicyMode::Allow);
        assert_eq!(project_config(second).agent_api.read_metadata, AgentPolicyMode::Prompt);
    }
}
