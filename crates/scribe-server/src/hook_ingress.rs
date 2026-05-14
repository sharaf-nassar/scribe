//! Hook event ingress — receives `HookEvent`s from `scribe-hook-helper` over
//! the existing server IPC socket and translates them into the same
//! `MetadataEvent` pipeline the deleted OSC 1337 parser used to feed.
//!
//! See `specs/003-ai-hook-channel/contracts/wire-protocol.md` for the
//! dispatch contract and `specs/003-ai-hook-channel/data-model.md` for the
//! `HookEventKind` → `MetadataEvent` mapping table.

use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
use scribe_common::hook::{
    CONVERSATION_ID_CAP_BYTES, HookEvent, HookEventKind, LAST_MESSAGE_CAP_BYTES,
    PROMPT_TEXT_CAP_BYTES, TASK_LABEL_CAP_BYTES,
};
use scribe_common::ids::SessionId;
use scribe_pty::metadata::MetadataEvent;

use crate::ipc_server::{ClientWriter, IpcServerState, send_metadata_event};
use crate::stop_classifier;

/// Handle one inbound `HookEvent` from a hook subprocess.
///
/// Invoked from the transient-message branch in `establish_client_window`.
/// After this returns, the caller closes the connection — no reply on the
/// wire (FR-007 helper side; server side reciprocates).
///
/// Drops the event silently on every error path (FR-014, FR-016):
///   - unknown `session_id`
///   - oversize payloads are truncated, never rejected
///   - all logging is `tracing::debug!` (never `error!`) — hook events are
///     advisory, never required.
pub async fn handle(server: &IpcServerState, event: HookEvent) {
    let HookEvent { session_id, provider, kind } = event;

    let Some(client_writer) = lookup_client_writer(server, session_id).await else {
        // `warn!` (not `debug!`) so this is visible at default log level —
        // it's the single most useful diagnostic when a user reports
        // "my hook indicator isn't updating".
        tracing::warn!(
            target: "scribe_server::hook_ingress",
            %session_id, ?provider,
            "hook event for unknown session — dropped"
        );
        return;
    };

    let Some(metadata_event) = translate(provider, kind) else {
        return;
    };

    send_metadata_event(
        metadata_event,
        session_id,
        &client_writer,
        &server.workspace_manager,
        &server.live_sessions,
    )
    .await;
}

/// Resolve the session's current client writer (may have no attached client,
/// in which case `send_to_client` silently no-ops; that's fine).
async fn lookup_client_writer(
    server: &IpcServerState,
    session_id: SessionId,
) -> Option<ClientWriter> {
    let sessions = server.live_sessions.read().await;
    sessions.get(&session_id).map(|session| std::sync::Arc::clone(&session.client_writer))
}

/// Translate a `HookEventKind` into a `MetadataEvent`. Applies field caps
/// (FR-016) and clamping (`fill_percent`). Returns `None` only for kinds
/// that have no downstream metadata representation (currently none — all
/// variants map to something).
fn translate(provider: AiProvider, kind: HookEventKind) -> Option<MetadataEvent> {
    match kind {
        HookEventKind::StateChanged { state, conversation_id } => {
            let ai_state = build_ai_state(provider, state, conversation_id);
            Some(MetadataEvent::AiStateChanged(ai_state))
        }

        HookEventKind::SessionStopped { last_message, conversation_id } => {
            let original_len = last_message.len();
            let truncated = truncate_chars(&last_message, LAST_MESSAGE_CAP_BYTES);
            let state = stop_classifier::classify(&truncated);
            tracing::debug!(
                target: "scribe_server::hook_ingress",
                ?provider, ?state,
                classified_bytes = truncated.len(),
                was_truncated = original_len > truncated.len(),
                "session_stopped classified"
            );
            let ai_state = build_ai_state(provider, state, conversation_id);
            Some(MetadataEvent::AiStateChanged(ai_state))
        }

        HookEventKind::StateCleared => Some(MetadataEvent::AiStateCleared),

        HookEventKind::PromptReceived { text, conversation_id: _ } => {
            let trimmed = truncate_chars(text.trim(), PROMPT_TEXT_CAP_BYTES);
            if trimmed.is_empty() {
                return None;
            }
            Some(MetadataEvent::PromptReceived { provider, text: trimmed })
        }

        HookEventKind::TaskLabelChanged { label } => {
            let trimmed = truncate_chars(label.trim(), TASK_LABEL_CAP_BYTES);
            if trimmed.is_empty() {
                return None;
            }
            Some(MetadataEvent::TaskLabelChanged { provider, label: trimmed })
        }

        HookEventKind::TaskLabelCleared => Some(MetadataEvent::TaskLabelCleared { provider }),

        HookEventKind::ContextChanged { fill_percent } => {
            let context = fill_percent.min(100);
            Some(MetadataEvent::AiContextChanged { provider, context })
        }
    }
}

/// Build an `AiProcessState` carrying the provider, state, and optional
/// sanitized `conversation_id`. Newlines and carriage returns are stripped
/// (defense-in-depth against producers that might smuggle key=value
/// injections or control chars through the field), then the result is
/// truncated to `CONVERSATION_ID_CAP_BYTES`. Other optional fields (`tool`,
/// `agent`, `model`, `context`) are left `None` and are merged in from the
/// existing live state by `merge_partial_ai_state` inside `send_metadata_event`.
fn build_ai_state(
    provider: AiProvider,
    state: AiState,
    conversation_id: Option<String>,
) -> AiProcessState {
    AiProcessState {
        provider,
        state,
        tool: None,
        agent: None,
        model: None,
        context: None,
        conversation_id: conversation_id.map(|s| sanitize_conversation_id(&s)),
    }
}

/// Strip line terminators and control bytes, then truncate to the
/// conversation-id cap. `\n` and `\r` are removed to ensure the value
/// cannot be smuggled into anything that parses by line, and other
/// ASCII control chars are dropped so the value stays display-safe.
fn sanitize_conversation_id(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != '\u{007f}')
        .take(CONVERSATION_ID_CAP_BYTES)
        .collect()
}

/// Truncate a string to at most `max_chars` Unicode characters. Mirrors the
/// helper in `scribe-pty::metadata::truncate_chars`, copied here to avoid
/// making that private helper public for a single caller.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::{build_ai_state, sanitize_conversation_id, translate};
    use scribe_common::ai_state::{AiProvider, AiState};
    use scribe_common::hook::{
        CONVERSATION_ID_CAP_BYTES, HookEventKind, LAST_MESSAGE_CAP_BYTES, PROMPT_TEXT_CAP_BYTES,
        TASK_LABEL_CAP_BYTES,
    };
    use scribe_pty::metadata::MetadataEvent;

    #[test]
    fn translate_state_changed_yields_ai_state_changed() {
        let event = HookEventKind::StateChanged {
            state: AiState::Processing,
            conversation_id: Some("conv-1".to_owned()),
        };
        let Some(MetadataEvent::AiStateChanged(ai_state)) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected AiStateChanged");
        };
        assert_eq!(ai_state.state, AiState::Processing);
        assert_eq!(ai_state.provider, AiProvider::ClaudeCode);
        assert_eq!(ai_state.conversation_id.as_deref(), Some("conv-1"));
    }

    #[test]
    fn translate_session_stopped_classifies_waiting_on_trailing_question() {
        let event = HookEventKind::SessionStopped {
            last_message: "Want me to proceed?".to_owned(),
            conversation_id: None,
        };
        let Some(MetadataEvent::AiStateChanged(ai_state)) = translate(AiProvider::CodexCode, event)
        else {
            panic!("expected AiStateChanged");
        };
        assert_eq!(ai_state.state, AiState::WaitingForInput);
    }

    #[test]
    fn translate_session_stopped_classifies_idle_on_completion() {
        let event = HookEventKind::SessionStopped {
            last_message: "Done. All set.".to_owned(),
            conversation_id: None,
        };
        let Some(MetadataEvent::AiStateChanged(ai_state)) = translate(AiProvider::Auggie, event)
        else {
            panic!("expected AiStateChanged");
        };
        assert_eq!(ai_state.state, AiState::IdlePrompt);
    }

    #[test]
    fn translate_state_cleared_yields_ai_state_cleared() {
        let event = HookEventKind::StateCleared;
        assert!(matches!(
            translate(AiProvider::Auggie, event),
            Some(MetadataEvent::AiStateCleared)
        ));
    }

    #[test]
    fn translate_prompt_received_drops_empty() {
        let event =
            HookEventKind::PromptReceived { text: "   \n  ".to_owned(), conversation_id: None };
        assert!(translate(AiProvider::ClaudeCode, event).is_none());
    }

    #[test]
    fn translate_prompt_received_truncates_oversize() {
        let oversize_text = "x".repeat(PROMPT_TEXT_CAP_BYTES + 50);
        let event = HookEventKind::PromptReceived { text: oversize_text, conversation_id: None };
        let Some(MetadataEvent::PromptReceived { text, .. }) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected PromptReceived");
        };
        assert_eq!(text.chars().count(), PROMPT_TEXT_CAP_BYTES);
    }

    #[test]
    fn translate_prompt_received_preserves_trimmed_content() {
        let event = HookEventKind::PromptReceived {
            text: "  Fix the login bug  ".to_owned(),
            conversation_id: None,
        };
        let Some(MetadataEvent::PromptReceived { text, provider }) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected PromptReceived");
        };
        assert_eq!(text, "Fix the login bug");
        assert_eq!(provider, AiProvider::ClaudeCode);
    }

    #[test]
    fn translate_task_label_changed_drops_empty_after_trim() {
        let event = HookEventKind::TaskLabelChanged { label: "   ".to_owned() };
        assert!(translate(AiProvider::CodexCode, event).is_none());
    }

    #[test]
    fn translate_task_label_changed_truncates_oversize() {
        let oversize_label = "L".repeat(TASK_LABEL_CAP_BYTES + 50);
        let event = HookEventKind::TaskLabelChanged { label: oversize_label };
        let Some(MetadataEvent::TaskLabelChanged { label, provider }) =
            translate(AiProvider::CodexCode, event)
        else {
            panic!("expected TaskLabelChanged");
        };
        assert_eq!(label.chars().count(), TASK_LABEL_CAP_BYTES);
        assert_eq!(provider, AiProvider::CodexCode);
    }

    #[test]
    fn translate_task_label_cleared_preserves_provider() {
        let event = HookEventKind::TaskLabelCleared;
        let Some(MetadataEvent::TaskLabelCleared { provider }) =
            translate(AiProvider::Auggie, event)
        else {
            panic!("expected TaskLabelCleared");
        };
        assert_eq!(provider, AiProvider::Auggie);
    }

    #[test]
    fn translate_context_changed_clamps_above_100() {
        let event = HookEventKind::ContextChanged { fill_percent: 200 };
        let Some(MetadataEvent::AiContextChanged { provider, context }) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected AiContextChanged");
        };
        assert_eq!(context, 100);
        assert_eq!(provider, AiProvider::ClaudeCode);
    }

    #[test]
    fn translate_context_changed_passes_through_valid() {
        let event = HookEventKind::ContextChanged { fill_percent: 73 };
        let Some(MetadataEvent::AiContextChanged { context, .. }) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected AiContextChanged");
        };
        assert_eq!(context, 73);
    }

    #[test]
    fn build_ai_state_strips_newlines_from_conversation_id() {
        let ai_state = build_ai_state(
            AiProvider::Auggie,
            AiState::Processing,
            Some("real-id\nlabel=injected".to_owned()),
        );
        // The injection attempt above would inject a fake key=value line in
        // shell-side parsers. Stripping newlines server-side is the
        // defense-in-depth complement to the adapter's own newline strip.
        let cid = ai_state.conversation_id.expect("conversation_id set");
        assert!(!cid.contains('\n'));
        assert!(!cid.contains('\r'));
        assert!(cid.starts_with("real-id"));
    }

    #[test]
    fn sanitize_conversation_id_truncates_to_cap() {
        let oversize: String = "a".repeat(CONVERSATION_ID_CAP_BYTES + 100);
        let sanitized = sanitize_conversation_id(&oversize);
        assert_eq!(sanitized.chars().count(), CONVERSATION_ID_CAP_BYTES);
    }

    #[test]
    fn sanitize_conversation_id_drops_control_chars() {
        let s = "abc\x07\x1b[31mred\x1b[0m";
        let sanitized = sanitize_conversation_id(s);
        assert_eq!(sanitized, "abc[31mred[0m");
    }

    #[test]
    fn translate_session_stopped_truncates_oversize_message() {
        // 1.5x cap of plain ASCII — every byte is one Unicode scalar so
        // bytes==chars and the truncate at chars cap clips at the same
        // length you'd expect from bytes.
        let last_message = "x".repeat(LAST_MESSAGE_CAP_BYTES + 5_000);
        let event = HookEventKind::SessionStopped { last_message, conversation_id: None };
        // The interesting assertion is that translate doesn't panic on
        // oversize input and that the classifier still runs. Default
        // classifier verdict for all-`x` is IdlePrompt.
        let Some(MetadataEvent::AiStateChanged(ai_state)) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected AiStateChanged");
        };
        assert_eq!(ai_state.state, AiState::IdlePrompt);
    }

    #[test]
    fn translate_codex_task_label_routes_via_codex_variant() {
        // Per the mapping in data-model.md, TaskLabelChanged{provider=Codex}
        // becomes MetadataEvent::TaskLabelChanged with provider=CodexCode —
        // convert_metadata_event (called downstream by send_metadata_event)
        // is responsible for splitting Codex out into a CodexTaskLabelChanged
        // ServerMessage. We only assert the MetadataEvent carries the
        // right provider.
        let event = HookEventKind::TaskLabelChanged { label: "Ship a thing".to_owned() };
        let Some(MetadataEvent::TaskLabelChanged { provider, label }) =
            translate(AiProvider::CodexCode, event)
        else {
            panic!("expected TaskLabelChanged");
        };
        assert_eq!(provider, AiProvider::CodexCode);
        assert_eq!(label, "Ship a thing");
    }
}
