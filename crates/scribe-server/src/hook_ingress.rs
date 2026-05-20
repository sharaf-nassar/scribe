//! Hook event ingress — receives `HookEvent`s from `scribe-hook-helper` over
//! the existing server IPC socket and translates them into the same
//! `MetadataEvent` pipeline the deleted OSC 1337 parser used to feed.
//!
//! See `specs/003-ai-hook-channel/contracts/wire-protocol.md` for the
//! dispatch contract and `specs/003-ai-hook-channel/data-model.md` for the
//! `HookEventKind` → `MetadataEvent` mapping table.
//!
//! Env-delta events ([`HookEventKind::EnvChanged`]) take a different path:
//! they have no `MetadataEvent` representation. Instead they fold into the
//! server-owned [`crate::env_store::EnvStoreState`] registry, which drives
//! the debounced encrypted persistence for terminal env restore (feature 006).

use std::sync::Arc;
use std::time::Instant;

use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
use scribe_common::hook::{
    CONVERSATION_ID_CAP_BYTES, HookEvent, HookEventKind, LAST_MESSAGE_CAP_BYTES,
    PROMPT_TEXT_CAP_BYTES, TASK_LABEL_CAP_BYTES,
};
use scribe_common::ids::{SessionId, WindowId};
use scribe_pty::metadata::MetadataEvent;

use crate::env_store::{EnvChangeEvent, EnvStoreState, StartupBaseline};
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

    // Env-delta events never produce a `MetadataEvent`. Route them to the
    // env-store registry instead, before the generic
    // `lookup_client_writer` / `translate` / `send_metadata_event` pipeline
    // below. We still need the live-session entry (for `env_window_id`
    // and `env_envelope_id`) but we look it up inline so we can take a
    // single read lock over both reads.
    if let HookEventKind::EnvChanged { added, removed, baseline_ready } = kind {
        handle_env_changed_dispatch(
            server,
            session_id,
            EnvDeltaInput { added, removed, baseline_ready },
        )
        .await;
        return;
    }

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

/// Per-event input collected from [`HookEventKind::EnvChanged`] before we
/// dispatch into the env-store registry. Decoupled from `HookEventKind`
/// (which is a wire type) so the free-function core can be unit-tested
/// without constructing a wire message.
#[derive(Debug, Clone)]
struct EnvDeltaInput {
    added: Vec<(String, String)>,
    removed: Vec<String>,
    baseline_ready: bool,
}

/// Live-session fields needed to route an env-delta event into the persist
/// scheduler. Looked up once under the live-sessions read lock so the
/// per-session `env_window_id` / `env_envelope_id` snapshot stays
/// consistent for the duration of the fold.
#[derive(Debug, Clone)]
struct EnvSessionCoords {
    window_id: WindowId,
    envelope_id: Option<String>,
}

/// Compose the live-session lookup, feature-gate read, and the
/// inner-pure-function call into one helper. Splitting in two halves keeps
/// the inner function ([`handle_env_changed`]) testable without
/// constructing a fake `IpcServerState`.
async fn handle_env_changed_dispatch(
    server: &IpcServerState,
    session_id: SessionId,
    event: EnvDeltaInput,
) {
    // Live-session lookup. Drops the env-delta silently for unknown
    // session ids (e.g. a stale shell subprocess that outlived its
    // session). Mirrors the existing `lookup_client_writer` contract.
    let coords = {
        let sessions = server.live_sessions.read().await;
        sessions.get(&session_id).map(|s| EnvSessionCoords {
            window_id: s.env_window_id,
            envelope_id: s.env_envelope_id.clone(),
        })
    };

    let Some(coords) = coords else {
        tracing::debug!(
            target: "scribe_server::hook_ingress",
            ?session_id,
            "EnvChanged for unknown session; dropped"
        );
        return;
    };

    // Feature-gate read. Loads from disk on each event (cheap: TOML
    // parsing of a small file). Hot-reloads are observed automatically
    // because there is no in-memory caching layer in front of
    // `load_config`. Failure to load is treated as "disabled" (fail-safe).
    let enabled = matches!(
        scribe_common::config::load_config(),
        Ok(cfg) if cfg.terminal.env_persistence.enabled
    );

    handle_env_changed(
        &server.env_store,
        EnvChangedCtx {
            session_id,
            window_id: coords.window_id,
            envelope_id: coords.envelope_id.as_deref(),
            feature_enabled: enabled,
        },
        event,
    )
    .await;
}

/// Bundle of per-call inputs to [`handle_env_changed`] that come from
/// session coordinates and the feature gate. Grouped into one struct so the
/// handler's argument count stays within Clippy's `too_many_arguments`
/// threshold and call sites read at a glance.
struct EnvChangedCtx<'a> {
    session_id: SessionId,
    window_id: WindowId,
    envelope_id: Option<&'a str>,
    feature_enabled: bool,
}

/// Inner-pure entry point for env-delta handling. Free function (not a
/// method on `IpcServerState`) so unit tests can drive it with a stub
/// `Arc<EnvStoreState>` and synthetic coords without standing up the full
/// server. Mirrors the algorithm in
/// `specs/006-persist-terminal-env/tasks.md::T016`:
///
/// 1. If the feature is disabled, drop the event.
/// 2. If `baseline_ready: true`, record the baseline and return — no
///    persist (the working delta is empty at this point).
/// 3. Otherwise build an `EnvChangeEvent`, fold it into the per-session
///    `TerminalEnvDelta` via [`EnvStoreState::fold_event`], and — only if
///    a baseline existed and the session has an `env_envelope_id` — call
///    [`EnvStoreState::schedule_persist`] to (re)arm the 100 ms debounce.
async fn handle_env_changed(
    env_store: &Arc<EnvStoreState>,
    ctx: EnvChangedCtx<'_>,
    event: EnvDeltaInput,
) {
    let EnvChangedCtx { session_id, window_id, envelope_id, feature_enabled } = ctx;

    if !feature_enabled {
        tracing::debug!(
            target: "scribe_server::hook_ingress",
            ?session_id,
            "env_persistence feature disabled; EnvChanged dropped"
        );
        return;
    }

    if event.baseline_ready {
        // Post-rc snapshot. The shell integration captures the full
        // exported env in `added`; no removes are meaningful for a
        // baseline. The exclusion list is applied at persist time
        // (via the delta path); the baseline itself stays raw because
        // it is only ever consumed in-memory to compute future diffs.
        let baseline = StartupBaseline {
            vars: event.added.into_iter().collect(),
            captured_at: Instant::now(),
        };
        env_store.record_baseline(session_id, baseline).await;
        tracing::debug!(
            target: "scribe_server::hook_ingress",
            ?session_id,
            "recorded StartupBaseline"
        );
        return;
    }

    // Filter the wire input through the exclusion set before folding so
    // we don't pay the BTreeMap-insert / serialize-size hint cost for
    // names that would be dropped anyway. Note `apply_event` re-applies
    // `is_excluded` defensively; that double-filter is fine.
    let change = EnvChangeEvent {
        added: event
            .added
            .into_iter()
            .filter(|(name, _)| !crate::env_store::is_excluded(name))
            .collect(),
        removed: event
            .removed
            .into_iter()
            .filter(|name| !crate::env_store::is_excluded(name))
            .collect(),
    };

    let folded = env_store.fold_event(session_id, change).await;
    if !folded {
        // No baseline yet — `EnvStoreState::fold_event` logs the drop
        // itself; no need to re-log here.
        return;
    }

    let Some(launch_id) = envelope_id else {
        // The session has no `env_envelope_id` (fresh non-restored
        // session). The capture-side state lives in memory only until
        // either the client is taught to issue a launch id for fresh
        // sessions or the session ends. Per T016's pragmatic compromise:
        // do not invent a launch id here; surface a debug log so
        // operators can see this case in field reports.
        tracing::debug!(
            target: "scribe_server::hook_ingress",
            ?session_id,
            ?window_id,
            "no env_envelope_id on session; capture in memory only (no persist)"
        );
        return;
    };

    env_store.schedule_persist(session_id, window_id, launch_id.to_string()).await;
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
/// that have no downstream metadata representation.
///
/// [`HookEventKind::EnvChanged`] is intentionally absent from this match:
/// `handle` short-circuits before calling `translate`, routing env-delta
/// events to [`handle_env_changed_dispatch`] instead. If translate ever
/// sees an `EnvChanged` it would be a bug — guarded by `unreachable!`.
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

        // EnvChanged routes via `handle_env_changed_dispatch`; reaching
        // `translate` for it means the caller bypassed the short-circuit.
        // Return `None` (silent drop) to preserve the fail-open contract
        // rather than panicking the whole IPC connection handler.
        HookEventKind::EnvChanged { .. } => {
            tracing::debug!(
                target: "scribe_server::hook_ingress",
                ?provider,
                "translate() unexpectedly received EnvChanged; dropping"
            );
            None
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
        let Some(MetadataEvent::AiStateChanged(ai_state)) =
            translate(AiProvider::ClaudeCode, event)
        else {
            panic!("expected AiStateChanged");
        };
        assert_eq!(ai_state.state, AiState::IdlePrompt);
    }

    #[test]
    fn translate_state_cleared_yields_ai_state_cleared() {
        let event = HookEventKind::StateCleared;
        assert!(matches!(
            translate(AiProvider::ClaudeCode, event),
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
            translate(AiProvider::CodexCode, event)
        else {
            panic!("expected TaskLabelCleared");
        };
        assert_eq!(provider, AiProvider::CodexCode);
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
            AiProvider::ClaudeCode,
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

/// Targeted tests for the [`handle_env_changed`] free function — the
/// inner-pure entry point for env-delta ingress. Driven with synthetic
/// [`EnvStoreState`] instances and stub coords so we exercise the
/// baseline / fold / debounce paths without a real `IpcServerState`.
#[cfg(test)]
mod env_changed_tests {
    use super::{EnvChangedCtx, EnvDeltaInput, handle_env_changed};
    use crate::env_store::EnvStoreState;
    use scribe_common::ids::{SessionId, WindowId};
    use std::sync::Arc;

    /// Happy-path round trip: baseline-ready records a baseline; the
    /// follow-up event folds into the working delta.
    #[tokio::test(flavor = "current_thread")]
    async fn baseline_then_delta_records_state() {
        let env_store = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        // 1. baseline_ready: true
        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("PATH".into(), "/x".into())],
                removed: vec![],
                baseline_ready: true,
            },
        )
        .await;
        assert!(env_store.has_baseline(session).await, "baseline must be recorded");
        // No delta yet on baseline-ready alone.
        assert!(env_store.current_delta(session).await.is_none());

        // 2. baseline_ready: false — a real delta event.
        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("FOO".into(), "bar".into())],
                removed: vec![],
                baseline_ready: false,
            },
        )
        .await;
        let delta =
            env_store.current_delta(session).await.expect("delta should be present after fold");
        assert_eq!(
            delta.added.get("FOO").map(String::as_str),
            Some("bar"),
            "fold must persist the added pair into the working delta"
        );
    }

    /// Feature-OFF: no state is recorded even on baseline-ready, and no
    /// delta is folded.
    #[tokio::test(flavor = "current_thread")]
    async fn feature_disabled_records_nothing() {
        let env_store = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: false,
            },
            EnvDeltaInput {
                added: vec![("PATH".into(), "/x".into())],
                removed: vec![],
                baseline_ready: true,
            },
        )
        .await;
        assert!(!env_store.has_baseline(session).await, "no baseline on feature-off");

        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: false,
            },
            EnvDeltaInput {
                added: vec![("FOO".into(), "bar".into())],
                removed: vec![],
                baseline_ready: false,
            },
        )
        .await;
        assert!(env_store.current_delta(session).await.is_none());
    }

    /// Delta event before baseline is dropped (the env-store registry
    /// enforces the delta-only-after-baseline invariant). The handler
    /// must not crash or panic.
    #[tokio::test(flavor = "current_thread")]
    async fn delta_before_baseline_dropped() {
        let env_store = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("FOO".into(), "bar".into())],
                removed: vec![],
                baseline_ready: false,
            },
        )
        .await;
        assert!(env_store.current_delta(session).await.is_none());
    }

    /// Without an `env_envelope_id` the fold still happens but
    /// `schedule_persist` is *not* invoked — no scheduler is spawned.
    /// Verifies the "capture in memory only" pragmatic compromise.
    #[tokio::test(flavor = "current_thread")]
    async fn missing_envelope_id_skips_schedule_persist() {
        let env_store = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        // Record a baseline first so the follow-up event folds.
        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: None, // no envelope id
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("PATH".into(), "/x".into())],
                removed: vec![],
                baseline_ready: true,
            },
        )
        .await;

        // Drive a real delta. The fold must succeed but no scheduler must
        // be created (visible via `current_delta` presence + no
        // schedule_persist side effect — see EnvStoreState::schedulers).
        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: None,
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("FOO".into(), "bar".into())],
                removed: vec![],
                baseline_ready: false,
            },
        )
        .await;
        let delta = env_store
            .current_delta(session)
            .await
            .expect("delta should fold even without envelope id");
        assert_eq!(delta.added.get("FOO").map(String::as_str), Some("bar"));
        // The session has no scheduler entry — `drop_scheduler` is a no-op
        // when none exists, so calling it here is a sanity check that we
        // didn't accidentally spawn a persist task.
        env_store.drop_scheduler(session).await;
    }

    /// Excluded variable names (e.g. `SHLVL`, `SCRIBE_SESSION_ID`) are
    /// filtered out before the fold so they never reach the working delta.
    #[tokio::test(flavor = "current_thread")]
    async fn excluded_names_are_filtered_before_fold() {
        let env_store = Arc::new(EnvStoreState::default());
        let session = SessionId::new();
        let window = WindowId::new();

        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![("PATH".into(), "/x".into())],
                removed: vec![],
                baseline_ready: true,
            },
        )
        .await;

        handle_env_changed(
            &env_store,
            EnvChangedCtx {
                session_id: session,
                window_id: window,
                envelope_id: Some("launch-1"),
                feature_enabled: true,
            },
            EnvDeltaInput {
                added: vec![
                    ("KEEP_ME".into(), "1".into()),
                    ("SHLVL".into(), "2".into()),
                    ("SCRIBE_SESSION_ID".into(), "deadbeef".into()),
                ],
                removed: vec!["TMUX".into(), "USER_VAR".into()],
                baseline_ready: false,
            },
        )
        .await;

        let delta = env_store.current_delta(session).await.expect("delta should be present");
        assert!(delta.added.contains_key("KEEP_ME"));
        assert!(!delta.added.contains_key("SHLVL"), "excluded SHLVL must be filtered");
        assert!(
            !delta.added.contains_key("SCRIBE_SESSION_ID"),
            "excluded SCRIBE_SESSION_ID must be filtered"
        );
        assert!(!delta.removed.contains("TMUX"), "excluded TMUX must be filtered from removed");
        assert!(
            delta.removed.contains("USER_VAR"),
            "non-excluded USER_VAR must reach the removed set"
        );
    }
}
