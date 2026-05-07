use serde::{Deserialize, Serialize};

/// Which AI assistant emitted a terminal integration state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    ClaudeCode,
    CodexCode,
    #[serde(rename = "auggie", alias = "auggie_code")]
    Auggie,
}

fn default_ai_provider() -> AiProvider {
    AiProvider::ClaudeCode
}

const AI_PROVIDERS: [AiProvider; 3] =
    [AiProvider::ClaudeCode, AiProvider::CodexCode, AiProvider::Auggie];

impl AiProvider {
    #[must_use]
    pub fn all() -> &'static [AiProvider] {
        &AI_PROVIDERS
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "claude_code",
            AiProvider::CodexCode => "codex_code",
            AiProvider::Auggie => "auggie",
        }
    }

    /// Inverse of [`Self::id`]. Used by the OSC 1337 `ScribeAiLaunch=<id>`
    /// pre-arm sentinel so shell integration can re-arm the ED 3 filter
    /// before an AI binary starts emitting bytes.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::all().iter().copied().find(|p| p.id() == id)
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "Claude Code",
            AiProvider::CodexCode => "Codex",
            AiProvider::Auggie => "Auggie",
        }
    }

    #[must_use]
    pub fn binary_name(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "claude",
            AiProvider::CodexCode => "codex",
            AiProvider::Auggie => "auggie",
        }
    }

    #[must_use]
    pub fn state_osc_key(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "ClaudeState",
            AiProvider::CodexCode => "CodexState",
            AiProvider::Auggie => "AuggieState",
        }
    }

    #[must_use]
    pub fn prompt_osc_key(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "ClaudePrompt",
            AiProvider::CodexCode => "CodexPrompt",
            AiProvider::Auggie => "AuggiePrompt",
        }
    }

    #[must_use]
    pub fn task_label_osc_key(self) -> Option<&'static str> {
        match self {
            AiProvider::ClaudeCode => None,
            AiProvider::CodexCode => Some("CodexTaskLabel"),
            AiProvider::Auggie => Some("AuggieTaskLabel"),
        }
    }

    #[must_use]
    pub fn resume_args(self) -> &'static [&'static str] {
        match self {
            AiProvider::ClaudeCode | AiProvider::Auggie => &["--resume"],
            AiProvider::CodexCode => &["resume"],
        }
    }
}

/// Core AI process states emitted by AI coding CLIs via OSC 1337 hooks.
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
/// Parsed from: `ESC ] 1337 ; <Provider>State=<state> [; key=value]... ST`
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
    /// Conversation identifier to resume this AI context.
    pub conversation_id: Option<String>,
}

impl AiProcessState {
    #[must_use]
    pub fn new(state: AiState) -> Self {
        Self::new_with_provider(AiProvider::ClaudeCode, state)
    }

    #[must_use]
    pub fn new_with_provider(provider: AiProvider, state: AiState) -> Self {
        Self {
            provider,
            state,
            tool: None,
            agent: None,
            model: None,
            context: None,
            conversation_id: None,
        }
    }

    /// Carry forward optional metadata from a previous same-provider state
    /// when the new event left those fields unset.
    ///
    /// The OSC 1337 protocol treats every `<Provider>State=...` event as a
    /// full snapshot, but state-only hooks (`PreToolUse`, `Notification`,
    /// `PostToolUse`, etc.) emit just the state with no `context=`,
    /// `model=`, or other metadata. Without this merge, every hook firing
    /// would clobber the values that the statusLine producer just set,
    /// hiding the live AI context % between hook events.
    ///
    /// Fields the new event explicitly carries are kept as-is. If the
    /// previous state belongs to a different provider (e.g. Claude →
    /// Codex), nothing is merged: switching providers starts fresh.
    pub fn merge_partial_from_previous(&mut self, prev: &Self) {
        if prev.provider != self.provider {
            return;
        }
        if self.context.is_none() {
            self.context = prev.context;
        }
        if self.model.is_none() {
            self.model.clone_from(&prev.model);
        }
        if self.tool.is_none() {
            self.tool.clone_from(&prev.tool);
        }
        if self.agent.is_none() {
            self.agent.clone_from(&prev.agent);
        }
        if self.conversation_id.is_none() {
            self.conversation_id.clone_from(&prev.conversation_id);
        }
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
