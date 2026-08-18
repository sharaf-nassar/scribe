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
    Pi,
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
const AI_PROVIDERS: [AiProvider; 3] =
    [AiProvider::ClaudeCode, AiProvider::CodexCode, AiProvider::Pi];

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
            AiProvider::Pi => "pi",
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
            AiProvider::Pi => "Pi",
            AiProvider::System => "System",
        }
    }

    #[must_use]
    pub fn binary_name(self) -> &'static str {
        match self {
            AiProvider::ClaudeCode => "claude",
            AiProvider::CodexCode => "codex",
            AiProvider::Pi => "pi",
            // No binary represents the System sentinel. Returning an empty
            // string is safe because the only callers (AI command
            // detection, new-tab launchers) iterate [`Self::all`], which
            // excludes `System`.
            AiProvider::System => "",
        }
    }

    #[must_use]
    pub fn supports_resume(self) -> bool {
        matches!(self, AiProvider::ClaudeCode | AiProvider::CodexCode)
    }

    #[must_use]
    pub fn resume_args(self) -> &'static [&'static str] {
        match self {
            AiProvider::ClaudeCode => &["--resume"],
            AiProvider::CodexCode => &["resume"],
            AiProvider::Pi | AiProvider::System => &[],
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
    /// Codex) or to a different conversation, nothing is merged: both
    /// switches start fresh. The conversation guard is what lets the
    /// context meter actually reset — a new conversation opens an empty
    /// context window, and merging the retired conversation's `context`
    /// into its first state-only hook would put the old fill straight back
    /// after the client cleared it.
    pub fn merge_partial_from_previous(&mut self, prev: &Self) {
        if prev.provider != self.provider || self.switched_conversation_from(prev) {
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

    /// Whether this state names a *different* conversation than `prev`.
    ///
    /// Only two named ids can disagree: an event that omits the id says
    /// nothing about which conversation it belongs to, and a first sighting
    /// is not a switch — the same rule the client's own conversation
    /// bookkeeping applies before it retires a pane's prompt bar.
    #[must_use]
    pub fn switched_conversation_from(&self, prev: &Self) -> bool {
        matches!(
            (&self.conversation_id, &prev.conversation_id),
            (Some(new), Some(old)) if new != old
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{AiProcessState, AiProvider, AiState};

    // @lat: [[test#Test Harness#Pi Provider Compatibility#Provider identity and config]]
    #[test]
    fn pi_is_user_visible_and_does_not_resume() {
        assert_eq!(AiProvider::Pi.id(), "pi");
        assert_eq!(AiProvider::from_id("pi"), Some(AiProvider::Pi));
        assert_eq!(AiProvider::Pi.display_name(), "Pi");
        assert_eq!(AiProvider::Pi.binary_name(), "pi");
        assert!(!AiProvider::Pi.supports_resume());
        assert_eq!(AiProvider::Pi.resume_args(), &[] as &[&str]);
        assert!(AiProvider::all().contains(&AiProvider::Pi));
        assert!(!AiProvider::all().contains(&AiProvider::System));
        assert!(AiProvider::ClaudeCode.supports_resume());
        assert!(AiProvider::CodexCode.supports_resume());
        assert!(!AiProvider::System.supports_resume());
    }

    #[test]
    fn pi_provider_uses_the_stable_serde_id() {
        let encoded = rmp_serde::to_vec_named(&AiProvider::Pi).expect("Pi provider serializes");
        let id: String = rmp_serde::from_slice(&encoded).expect("provider id decodes as text");
        assert_eq!(id, "pi");
        let decoded: AiProvider =
            rmp_serde::from_slice(&encoded).expect("Pi provider deserializes");
        assert_eq!(decoded, AiProvider::Pi);
    }

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

    // @lat: [[lat.md/common#Common#AI State#A conversation switch breaks the metadata merge]]
    #[test]
    fn conversation_switch_breaks_the_metadata_merge() {
        let mut prev = AiProcessState::new(AiState::Processing);
        prev.context = Some(80);
        prev.model = Some("opus".to_owned());
        prev.conversation_id = Some("conv-42".to_owned());

        // A state-only hook from the same conversation keeps the live fill.
        let mut same = AiProcessState::new(AiState::IdlePrompt);
        same.merge_partial_from_previous(&prev);
        assert_eq!(same.context, Some(80));
        assert_eq!(same.conversation_id.as_deref(), Some("conv-42"));

        // The same hook naming a new conversation inherits nothing: the new
        // window starts empty and the meter must not read 80%.
        let mut switched = AiProcessState::new(AiState::IdlePrompt);
        switched.conversation_id = Some("conv-43".to_owned());
        switched.merge_partial_from_previous(&prev);
        assert_eq!(switched.context, None);
        assert_eq!(switched.model, None);
        assert_eq!(switched.conversation_id.as_deref(), Some("conv-43"));
    }
}
