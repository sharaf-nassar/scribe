use serde::{Deserialize, Serialize};

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
        Self { state, tool: None, agent: None, model: None, context: None }
    }
}
