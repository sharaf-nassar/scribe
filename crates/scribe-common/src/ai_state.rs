use serde::{Deserialize, Serialize};

/// Which AI assistant emitted a terminal integration state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    ClaudeCode,
    CodexCode,
}

fn default_ai_provider() -> AiProvider {
    AiProvider::ClaudeCode
}

/// Core AI process states emitted by Claude Code via OSC 1337 hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiState {
    IdlePrompt,
    Processing,
    WaitingForInput,
    PermissionPrompt,
    Error,
}

/// Full AI process state with optional metadata keys.
/// Parsed from: `ESC ] 1337 ; ClaudeState=<state> [; key=value]... ST`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiProcessState {
    #[serde(default = "default_ai_provider")]
    pub provider: AiProvider,
    pub state: AiState,
    pub tool: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Context window usage percentage (0-100).
    pub context: Option<u8>,
}

impl AiProcessState {
    #[must_use]
    pub fn new(state: AiState) -> Self {
        Self::new_with_provider(AiProvider::ClaudeCode, state)
    }

    #[must_use]
    pub fn new_with_provider(provider: AiProvider, state: AiState) -> Self {
        Self { provider, state, tool: None, agent: None, model: None, context: None }
    }
}

#[cfg(test)]
mod tests {
    use super::{AiProcessState, AiProvider, AiState};

    #[test]
    fn deserializes_legacy_state_without_provider_as_claude() {
        let toml = r#"
state = "processing"
tool = "Bash"
model = "claude"
context = 42
"#;

        let state: AiProcessState =
            toml::from_str(toml).expect("legacy AI state should remain readable");

        assert_eq!(state.provider, AiProvider::ClaudeCode);
        assert_eq!(state.state, AiState::Processing);
        assert_eq!(state.tool.as_deref(), Some("Bash"));
        assert_eq!(state.model.as_deref(), Some("claude"));
        assert_eq!(state.context, Some(42));
    }
}
