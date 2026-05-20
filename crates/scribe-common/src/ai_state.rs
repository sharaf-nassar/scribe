use serde::{Deserialize, Serialize};

/// Which AI assistant emitted a terminal integration state change.
///
/// [`AiProvider::System`] is a sentinel for non-AI hook channel events
/// (currently env-delta from shell integration; future infrastructure
/// events). It is intentionally absent from [`AiProvider::all`] so UI
/// surfaces that list AI providers (pickers, new-tab menus, integration
/// settings) never display it. Hook ingress on the server is the one place
/// that may legitimately observe a `System` provider — handlers that route
/// by provider should pattern-match it explicitly and dispatch to the
/// non-AI path (e.g. env-store fold) or drop with a debug log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    ClaudeCode,
    CodexCode,
    /// Non-AI infrastructure events emitted by shell integration or the
    /// server itself. Carries env-delta hook events today; reserved for
    /// future structured hook channels that do not represent an AI tool.
    System,
}

fn default_ai_provider() -> AiProvider {
    AiProvider::ClaudeCode
}

/// Iterable set of AI provider variants. Intentionally excludes
/// [`AiProvider::System`] so UI listings (pickers, settings, new-tab
/// menus) never surface the synthetic provider.
const AI_PROVIDERS: [AiProvider; 2] = [AiProvider::ClaudeCode, AiProvider::CodexCode];

impl AiProvider {
    /// All *user-visible* AI providers. Does NOT include
    /// [`AiProvider::System`] — that variant is a hook-channel sentinel
    /// for non-AI events and must not appear in any UI surface.
    #[must_use]
    pub fn all() -> &'static [AiProvider] {
        &AI_PROVIDERS
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "claude_code",
            AiProvider::CodexCode => "codex_code",
            AiProvider::System => "system",
        }
    }

    /// Inverse of [`Self::id`]. Used by the OSC 1337 `ScribeAiLaunch=<id>`
    /// pre-arm sentinel so shell integration can re-arm the ED 3 filter
    /// before an AI binary starts emitting bytes.
    ///
    /// Also accepts the synthetic `"system"` id used by
    /// `scribe-hook-helper --provider=system` for env-delta events. Note
    /// `"system"` is intentionally NOT in [`Self::all`], so callers that
    /// rely on iteration (e.g. AI-binary detection, integration config)
    /// will not pick it up.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "system" => Some(AiProvider::System),
            _ => Self::all().iter().copied().find(|p| p.id() == id),
        }
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "Claude Code",
            AiProvider::CodexCode => "Codex",
            AiProvider::System => "System",
        }
    }

    #[must_use]
    pub fn binary_name(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "claude",
            AiProvider::CodexCode => "codex",
            // No binary represents the System sentinel. Returning an empty
            // string is safe because the only callers (AI command
            // detection, new-tab launchers) iterate [`Self::all`], which
            // excludes `System`.
            AiProvider::System => "",
        }
    }

    #[must_use]
    pub fn resume_args(self) -> &'static [&'static str] {
        match self {
            AiProvider::ClaudeCode => &["--resume"],
            AiProvider::CodexCode => &["resume"],
            // `System` has no resume semantics. Same rationale as
            // `binary_name`: it isn't in `all()`, so this arm is unreachable
            // via the normal launcher paths.
            AiProvider::System => &[],
        }
    }
}

/// Core AI process states emitted by supported AI coding CLIs.
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
