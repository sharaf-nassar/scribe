//! Hook channel event types — the wire format between `scribe-hook-helper`
//! (invoked by AI tool hook subprocesses) and `scribe-server`.
//!
//! See `specs/003-ai-hook-channel/data-model.md` and
//! `specs/003-ai-hook-channel/contracts/wire-protocol.md`.

use serde::{Deserialize, Serialize};

use crate::ai_state::{AiProvider, AiState};
use crate::ids::SessionId;

/// Cap on `SessionStopped::last_message` payload size.
///
/// Sized to comfortably hold the stop-classifier's "last ~20 non-empty lines"
/// window without being unbounded; the server truncates oversize messages
/// rather than rejecting them (FR-016).
pub const LAST_MESSAGE_CAP_BYTES: usize = 16_384;

/// Cap on `PromptReceived::text`. Matches the existing OSC 1337 prompt-text
/// cap so the new channel produces UI-identical results to the deleted path.
pub const PROMPT_TEXT_CAP_BYTES: usize = 256;

/// Cap on `TaskLabelChanged::label`. Matches the existing OSC 1337 task-label
/// cap.
pub const TASK_LABEL_CAP_BYTES: usize = 256;

/// Cap on the optional `conversation_id` field carried by several variants.
/// Conversation IDs are short opaque tokens (UUIDs in practice); this cap
/// bounds memory use even when a malicious or buggy producer sends a huge
/// value, and is distinct from `LAST_MESSAGE_CAP_BYTES` (16 KiB) so the two
/// fields don't share a misleading limit.
pub const CONVERSATION_ID_CAP_BYTES: usize = 256;

/// One hook event delivered over the channel.
///
/// Carries the routing key (`session_id`), the producing AI tool
/// (`provider`), and the event payload (`kind`). Serialized as a single
/// `ClientMessage::HookEvent(...)` frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// Scribe pane / PTY identity. Read by the helper from `SCRIBE_SESSION_ID`.
    pub session_id: SessionId,

    /// AI tool that emitted the event. Claimed by the helper from its
    /// `--provider=…` CLI arg. The server drops events whose provider is
    /// not recognized by the current build (FR-014).
    pub provider: AiProvider,

    /// Event payload variant.
    pub kind: HookEventKind,
}

/// Discriminated payload of a hook event.
///
/// Serialized as `{ "type": "<variant>", ...fields }` (msgpack-named via
/// `#[serde(tag = "type", rename_all = "snake_case")]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEventKind {
    /// Adapter observed a structured state transition (e.g.
    /// `PreToolUse:AskUserQuestion` → `WaitingForInput`).
    StateChanged {
        state: AiState,
        #[serde(default)]
        conversation_id: Option<String>,
    },

    /// AI tool's `Stop` hook fired. The server's `stop_classifier` maps the
    /// last-message text to `AiState::IdlePrompt` or `AiState::WaitingForInput`
    /// per FR-013a.
    SessionStopped {
        /// Truncated at `LAST_MESSAGE_CAP_BYTES` server-side when oversize.
        last_message: String,
        #[serde(default)]
        conversation_id: Option<String>,
    },

    /// AI session ended; clear live state for this (`session_id`, provider).
    StateCleared,

    /// User submitted a prompt. Feeds the prompt-bar.
    PromptReceived {
        /// Truncated at `PROMPT_TEXT_CAP_BYTES` server-side when oversize.
        text: String,
        #[serde(default)]
        conversation_id: Option<String>,
    },

    /// Sanitized task label for the tab strip.
    TaskLabelChanged {
        /// Truncated at `TASK_LABEL_CAP_BYTES` server-side when oversize.
        label: String,
    },

    /// Clear the tab-strip task label without changing AI state.
    TaskLabelCleared,

    /// Context-window fill percentage. Server clamps to 0..=100.
    ContextChanged { fill_percent: u8 },
}
