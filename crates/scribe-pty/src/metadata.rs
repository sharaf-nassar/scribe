use std::path::PathBuf;

use scribe_common::ai_state::{AiProcessState, AiProvider, AiState};
use scribe_common::protocol::{PromptMarkKind, SessionContext};

/// Maximum length for window title strings (chars). Longer titles are truncated.
const MAX_TITLE_LEN: usize = 4096;

/// Maximum length for AI metadata fields (tool, agent, model) in chars.
const MAX_AI_FIELD_LEN: usize = 256;

/// Maximum length for task labels emitted via hook metadata.
const MAX_TASK_LABEL_LEN: usize = 256;
/// Maximum length for prompt text emitted by AI CLIs.
const MAX_PROMPT_TEXT_LEN: usize = 256;
/// Maximum length for shell context fields (host, tmux session).
const MAX_CONTEXT_FIELD_LEN: usize = 256;

/// Events extracted from the PTY output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataEvent {
    CwdChanged(PathBuf),
    TitleChanged(String),
    SessionContextChanged(SessionContext),
    TaskLabelChanged {
        provider: AiProvider,
        label: String,
    },
    TaskLabelCleared {
        provider: AiProvider,
    },
    CodexTaskLabelChanged(String),
    CodexTaskLabelCleared,
    /// A user prompt was submitted in a supported AI coding session.
    PromptReceived {
        provider: AiProvider,
        text: String,
    },
    AiStateChanged(AiProcessState),
    /// The AI state was explicitly cleared (OSC 1337 `ClaudeState=inactive`).
    AiStateCleared,
    /// Context-window % refresh from a status-line / usage-poll producer.
    /// Carries no state — the server patches `context` on the live
    /// `AiProcessState` for the matching provider and re-broadcasts. If no
    /// state has been established yet (or it belongs to a different
    /// provider), the event is dropped to avoid synthesizing a fake state.
    AiContextChanged {
        provider: AiProvider,
        context: u8,
    },
    /// Shell-integration sentinel that pre-arms the ED 3 filter for the next
    /// command. Emitted by `__scribe_preexec` (zsh) / DEBUG trap (bash) /
    /// equivalents when the user runs `claude`, `codex`, or `auggie`. Lets
    /// `<tool> --resume` survive its pre-OSC-1337 ED 3 even after `ai_provider`
    /// has been cleared by an `AiStateCleared` from the previous run.
    AiProviderArmed {
        provider: AiProvider,
    },
    Bell,
    PromptMark {
        kind: PromptMarkKind,
        click_events: bool,
        exit_code: Option<i32>,
    },
}

/// Stateless parser helpers that extract OSC metadata from a VTE Perform implementation.
pub struct MetadataParser;

impl MetadataParser {
    /// Process an OSC sequence and return a metadata event if one was extracted.
    /// The `params` slice contains the semicolon-delimited parts.
    #[must_use]
    pub fn process_osc(params: &[&[u8]]) -> Option<MetadataEvent> {
        let osc_number = params.first()?;

        match *osc_number {
            b"0" | b"2" => Self::parse_title(params),
            b"7" => Self::parse_cwd(params),
            b"133" => Self::parse_prompt_mark(params),
            b"1337" => Self::parse_iterm2(params),
            _ => None,
        }
    }

    /// Process a C0 control byte and return a metadata event if applicable.
    #[must_use]
    pub fn process_execute(byte: u8) -> Option<MetadataEvent> {
        if byte == 0x07 { Some(MetadataEvent::Bell) } else { None }
    }

    fn parse_title(params: &[&[u8]]) -> Option<MetadataEvent> {
        let title_bytes = params.get(1)?;
        let title = truncate_chars(&String::from_utf8_lossy(title_bytes), MAX_TITLE_LEN);
        if title.trim().is_empty() {
            return None;
        }
        Some(MetadataEvent::TitleChanged(title))
    }

    fn parse_cwd(params: &[&[u8]]) -> Option<MetadataEvent> {
        let uri_bytes = params.get(1)?;
        let uri = String::from_utf8_lossy(uri_bytes);

        // OSC 7 payload is a file:// URI: file://hostname/path
        let raw_path = uri.strip_prefix("file://").map_or_else(
            || Some(uri.as_ref()),
            |stripped| stripped.find('/').and_then(|i| stripped.get(i..)),
        )?;

        // Percent-decode the URI path (e.g. %20 → space) before constructing
        // a PathBuf, then normalize and re-validate that the result is still
        // absolute (excessive ".." could reduce it to a relative path).
        let decoded = percent_decode_path(raw_path);
        let normalized = normalize_path(&PathBuf::from(decoded));

        if normalized.is_absolute() { Some(MetadataEvent::CwdChanged(normalized)) } else { None }
    }

    fn parse_prompt_mark(params: &[&[u8]]) -> Option<MetadataEvent> {
        // params[1] is the mark letter, possibly followed by key=value pairs
        // separated by semicolons (each as a separate VTE param element).
        let mark_bytes = params.get(1)?;
        let mark_str = String::from_utf8_lossy(mark_bytes);
        // The mark letter may be just "A" or "A;k=s" within params[1],
        // or extras arrive in params[2..] as separate elements.
        let (letter, inline_rest) =
            mark_str.split_once(';').map_or((mark_str.as_ref(), ""), |(a, b)| (a, b));

        let kind = match letter {
            "A" => PromptMarkKind::PromptStart,
            "B" => PromptMarkKind::PromptEnd,
            "C" => PromptMarkKind::CommandStart,
            "D" => PromptMarkKind::CommandEnd,
            _ => return None,
        };

        let mut click_events = false;
        let mut exit_code: Option<i32> = None;

        // Check inline key=value pairs from within params[1] (e.g. "A;k=s").
        for kv in inline_rest.split(';').filter(|s| !s.is_empty()) {
            parse_prompt_param(kv, &mut click_events, &mut exit_code);
        }

        // Check additional VTE params (params[2..]).
        for raw in params.get(2..).unwrap_or_default() {
            let kv = String::from_utf8_lossy(raw);
            parse_prompt_param(kv.as_ref(), &mut click_events, &mut exit_code);
        }

        // For D mark, the exit code may also be a bare number in params[2].
        if kind == PromptMarkKind::CommandEnd && exit_code.is_none() {
            if let Some(raw) = params.get(2) {
                let s = String::from_utf8_lossy(raw);
                exit_code = s.parse().ok();
            }
        }

        Some(MetadataEvent::PromptMark { kind, click_events, exit_code })
    }

    fn parse_iterm2(params: &[&[u8]]) -> Option<MetadataEvent> {
        let payload_bytes = params.get(1)?;
        let payload = String::from_utf8_lossy(payload_bytes);

        // Primary provider formats:
        //   ESC ] 1337 ; <Provider>State=<state> [; key=value ...] ST
        //   ESC ] 1337 ; <Provider>Prompt=<text> ST
        //   ESC ] 1337 ; <Provider>TaskLabel=<label> ST
        for provider in AiProvider::all() {
            if let Some(event) = Self::parse_provider_iterm2_payload(*provider, &payload, params) {
                return Some(event);
            }
        }
        if payload == "ScribeContext" || payload.starts_with("ScribeContext=") {
            return Some(Self::parse_session_context(payload.as_ref(), params));
        }

        if let Some(provider_id) = payload.strip_prefix("ScribeAiLaunch=")
            && let Some(provider) = AiProvider::from_id(provider_id.trim())
        {
            return Some(MetadataEvent::AiProviderArmed { provider });
        }

        // Legacy format: ESC ] 1337 ; AiState=state=<state>;key=val... ST
        // Kept for backwards compatibility with older Claude Code versions.
        if let Some(legacy_payload) = payload.strip_prefix("AiState=") {
            return Self::parse_legacy_ai_state(legacy_payload);
        }

        None
    }

    fn parse_provider_iterm2_payload(
        provider: AiProvider,
        payload: &str,
        params: &[&[u8]],
    ) -> Option<MetadataEvent> {
        let state_prefix = format!("{}=", provider.state_osc_key());
        if let Some(state_value) = payload.strip_prefix(&state_prefix) {
            return Self::parse_named_ai_state(provider, state_value, params);
        }

        let context_prefix = format!("{}=", provider.context_osc_key());
        if let Some(context_value) = payload.strip_prefix(&context_prefix) {
            return Self::parse_named_ai_context(provider, context_value);
        }

        let prompt_prefix = format!("{}=", provider.prompt_osc_key());
        if let Some(text) = payload.strip_prefix(&prompt_prefix) {
            return Self::parse_prompt(provider, text);
        }

        let task_label_key = provider.task_label_osc_key()?;
        let clear_payload = format!("{task_label_key}Cleared");
        if payload == clear_payload {
            return Some(MetadataEvent::TaskLabelCleared { provider });
        }

        let label_prefix = format!("{task_label_key}=");
        payload
            .strip_prefix(&label_prefix)
            .and_then(|label| Self::parse_task_label(provider, label))
    }

    /// Parse a `<Provider>Context=<u8>` payload. Values outside 0..=100 or
    /// non-numeric values are dropped (returns None) so a producer typo never
    /// corrupts the live context %.
    fn parse_named_ai_context(provider: AiProvider, value: &str) -> Option<MetadataEvent> {
        let context = value.trim().parse::<u8>().ok().filter(|v| *v <= 100)?;
        Some(MetadataEvent::AiContextChanged { provider, context })
    }

    /// Parse the legacy `AiState=state=X;key=val` single-payload format.
    fn parse_legacy_ai_state(payload: &str) -> Option<MetadataEvent> {
        let mut builder =
            AiStateBuilder { provider: AiProvider::ClaudeCode, ..AiStateBuilder::default() };
        for part in payload.split(';') {
            if let Some((key, value)) = part.split_once('=') {
                builder.apply(key, value);
            }
        }
        builder.build()
    }

    fn parse_named_ai_state(
        provider: AiProvider,
        state_value: &str,
        params: &[&[u8]],
    ) -> Option<MetadataEvent> {
        // "inactive" explicitly clears the AI state for this session.
        if state_value == "inactive" {
            return Some(MetadataEvent::AiStateCleared);
        }

        let state = match state_value {
            "idle_prompt" => AiState::IdlePrompt,
            "processing" => AiState::Processing,
            "waiting_for_input" => AiState::WaitingForInput,
            "permission_prompt" => AiState::PermissionPrompt,
            "error" => AiState::Error,
            _ => return None,
        };

        let mut builder =
            AiStateBuilder { provider, state: Some(state), ..AiStateBuilder::default() };

        // VTE splits OSC params on semicolons, so additional key=value
        // metadata (tool, agent, model, context) arrives in params[2..].
        for raw in params.get(2..).unwrap_or_default() {
            let kv = String::from_utf8_lossy(raw);
            if let Some((key, value)) = kv.split_once('=') {
                builder.apply(key, value);
            }
        }

        builder.build()
    }

    fn parse_task_label(provider: AiProvider, label: &str) -> Option<MetadataEvent> {
        let label = sanitize_text_payload(label, MAX_TASK_LABEL_LEN);
        if label.is_empty() {
            return None;
        }
        Some(MetadataEvent::TaskLabelChanged { provider, label })
    }

    fn parse_prompt(provider: AiProvider, text: &str) -> Option<MetadataEvent> {
        let text = sanitize_text_payload(text, MAX_PROMPT_TEXT_LEN);
        if text.is_empty() {
            return None;
        }
        Some(MetadataEvent::PromptReceived { provider, text })
    }

    fn parse_session_context(payload: &str, params: &[&[u8]]) -> MetadataEvent {
        let mut remote = false;
        let mut host = None;
        let mut tmux_session = None;

        if let Some(inline_kv) = payload.strip_prefix("ScribeContext=") {
            Self::apply_session_context_param(inline_kv, &mut remote, &mut host, &mut tmux_session);
        }

        for raw in params.get(2..).unwrap_or_default() {
            let kv = String::from_utf8_lossy(raw);
            Self::apply_session_context_param(
                kv.as_ref(),
                &mut remote,
                &mut host,
                &mut tmux_session,
            );
        }

        MetadataEvent::SessionContextChanged(SessionContext { remote, host, tmux_session })
    }

    fn apply_session_context_param(
        kv: &str,
        remote: &mut bool,
        host: &mut Option<String>,
        tmux_session: &mut Option<String>,
    ) {
        let Some((key, value)) = kv.split_once('=') else { return };
        let value = sanitize_text_payload(value, MAX_CONTEXT_FIELD_LEN);
        match key {
            "remote" => *remote = value == "1" || value.eq_ignore_ascii_case("true"),
            "host" if !value.is_empty() => *host = Some(value),
            "tmux" if !value.is_empty() => *tmux_session = Some(value),
            _ => {}
        }
    }
}

/// Parse a single key=value parameter from an OSC 133 sequence.
fn parse_prompt_param(kv: &str, click_events: &mut bool, exit_code: &mut Option<i32>) {
    if let Some((key, value)) = kv.split_once('=') {
        match key {
            "click_events" => *click_events = value == "1",
            "exit_code" => *exit_code = value.parse().ok(),
            _ => {}
        }
    }
}

/// Accumulates key=value fields from OSC 1337 `ClaudeState` params.
struct AiStateBuilder {
    provider: AiProvider,
    state: Option<AiState>,
    tool: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    context: Option<u8>,
    conversation_id: Option<String>,
}

impl Default for AiStateBuilder {
    fn default() -> Self {
        Self {
            provider: AiProvider::ClaudeCode,
            state: None,
            tool: None,
            agent: None,
            model: None,
            context: None,
            conversation_id: None,
        }
    }
}

impl AiStateBuilder {
    fn apply(&mut self, key: &str, value: &str) {
        match key {
            // "state" is used by the legacy `AiState=state=X;…` format where
            // all fields arrive in a single semicolon-delimited payload.
            "state" => {
                self.state = match value {
                    "idle_prompt" => Some(AiState::IdlePrompt),
                    "processing" => Some(AiState::Processing),
                    "waiting_for_input" => Some(AiState::WaitingForInput),
                    "permission_prompt" => Some(AiState::PermissionPrompt),
                    "error" => Some(AiState::Error),
                    _ => None,
                };
            }
            "tool" => self.tool = Some(truncate_chars(value, MAX_AI_FIELD_LEN)),
            "agent" => self.agent = Some(truncate_chars(value, MAX_AI_FIELD_LEN)),
            "model" => self.model = Some(truncate_chars(value, MAX_AI_FIELD_LEN)),
            "context" => {
                self.context = value.parse::<u8>().ok().filter(|v| *v <= 100);
            }
            "conversation_id" => {
                self.conversation_id = Some(sanitize_text_payload(value, MAX_AI_FIELD_LEN));
            }
            _ => {} // Ignore unknown keys (forward compatibility)
        }
    }

    fn build(self) -> Option<MetadataEvent> {
        let state = self.state?;
        Some(MetadataEvent::AiStateChanged(AiProcessState {
            provider: self.provider,
            state,
            tool: self.tool,
            agent: self.agent,
            model: self.model,
            context: self.context,
            conversation_id: self.conversation_id,
        }))
    }
}

/// Truncate a string to at most `max_chars` Unicode characters.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Remove control characters, trim whitespace, and truncate to a bounded size.
fn sanitize_text_payload(s: &str, max_chars: usize) -> String {
    let filtered: String = s.chars().filter(|ch| !ch.is_control()).collect();
    truncate_chars(filtered.trim(), max_chars)
}

/// Normalize a path by resolving `.` and `..` components without touching
/// the filesystem (no symlink resolution).
fn normalize_path(p: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Percent-decode a URI path component (e.g. `%20` → space, `%2F` → `/`).
///
/// Invalid `%XX` sequences (non-hex digits or truncated) are passed through
/// literally. The result is lossy-converted from bytes to a UTF-8 string.
fn percent_decode_path(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let Some(&b) = bytes.get(i) else { break };
        if b == b'%' {
            if let Some(decoded) =
                decode_percent_pair(bytes.get(i + 1).copied(), bytes.get(i + 2).copied())
            {
                out.push(decoded);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a `%XX` hex pair into a single byte.
fn decode_percent_pair(hi: Option<u8>, lo: Option<u8>) -> Option<u8> {
    let h = hex_digit_value(hi?)?;
    let l = hex_digit_value(lo?)?;
    Some(h << 4 | l)
}

/// Convert an ASCII hex digit to its numeric value (0–15).
fn hex_digit_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MetadataEvent, MetadataParser};
    use scribe_common::ai_state::{AiProvider, AiState};

    fn parse_iterm2(payload: &[&[u8]]) -> Option<MetadataEvent> {
        MetadataParser::process_osc(payload)
    }

    #[test]
    fn parses_codex_processing_state_with_conversation_id() {
        let event = parse_iterm2(&[
            b"1337",
            b"CodexState=processing",
            b"tool=Bash",
            b"conversation_id=abc-123",
        ]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::CodexCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.tool.as_deref(), Some("Bash"));
                assert_eq!(ai_state.conversation_id.as_deref(), Some("abc-123"));
            }
            other => panic!("expected Codex processing state, got {other:?}"),
        }
    }

    #[test]
    fn parses_codex_processing_state() {
        let event =
            parse_iterm2(&[b"1337", b"CodexState=processing", b"tool=Bash", b"model=gpt-5"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::CodexCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.tool.as_deref(), Some("Bash"));
                assert_eq!(ai_state.model.as_deref(), Some("gpt-5"));
            }
            other => panic!("expected Codex processing state, got {other:?}"),
        }
    }

    #[test]
    fn clears_ai_state_for_codex_inactive() {
        let event = parse_iterm2(&[b"1337", b"CodexState=inactive"]);

        assert!(matches!(event, Some(MetadataEvent::AiStateCleared)));
    }

    #[test]
    fn preserves_claude_provider_for_legacy_payloads() {
        let event = parse_iterm2(&[b"1337", b"AiState=state=waiting_for_input;tool=Read"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::ClaudeCode);
                assert_eq!(ai_state.state, AiState::WaitingForInput);
                assert_eq!(ai_state.tool.as_deref(), Some("Read"));
            }
            other => panic!("expected legacy Claude state, got {other:?}"),
        }
    }

    #[test]
    fn parses_session_context_from_shell_integration_params() {
        let event = parse_iterm2(&[
            b"1337",
            b"ScribeContext",
            b"remote=1",
            b"host=builder",
            b"tmux=editor",
        ]);

        match event {
            Some(MetadataEvent::SessionContextChanged(context)) => {
                assert!(context.remote);
                assert_eq!(context.host.as_deref(), Some("builder"));
                assert_eq!(context.tmux_session.as_deref(), Some("editor"));
            }
            other => panic!("expected session context event, got {other:?}"),
        }
    }

    #[test]
    fn parses_session_context_from_legacy_prefixed_payload() {
        let event =
            parse_iterm2(&[b"1337", b"ScribeContext=remote=1", b"host=builder", b"tmux=editor"]);

        match event {
            Some(MetadataEvent::SessionContextChanged(context)) => {
                assert!(context.remote);
                assert_eq!(context.host.as_deref(), Some("builder"));
                assert_eq!(context.tmux_session.as_deref(), Some("editor"));
            }
            other => panic!("expected legacy session context event, got {other:?}"),
        }
    }

    #[test]
    fn parses_claude_prompt() {
        let event = parse_iterm2(&[b"1337", b"ClaudePrompt=Fix the login bug"]);
        match event {
            Some(MetadataEvent::PromptReceived { provider, text }) => {
                assert_eq!(provider, AiProvider::ClaudeCode);
                assert_eq!(text, "Fix the login bug");
            }
            other => panic!("expected PromptReceived, got {other:?}"),
        }
    }

    #[test]
    fn parses_codex_prompt() {
        let event = parse_iterm2(&[b"1337", b"CodexPrompt=Add OAuth support"]);
        match event {
            Some(MetadataEvent::PromptReceived { provider, text }) => {
                assert_eq!(provider, AiProvider::CodexCode);
                assert_eq!(text, "Add OAuth support");
            }
            other => panic!("expected PromptReceived, got {other:?}"),
        }
    }

    #[test]
    fn parses_auggie_prompt() {
        let event = parse_iterm2(&[b"1337", b"AuggiePrompt=Trace the config reload bug"]);
        match event {
            Some(MetadataEvent::PromptReceived { provider, text }) => {
                assert_eq!(provider, AiProvider::Auggie);
                assert_eq!(text, "Trace the config reload bug");
            }
            other => panic!("expected Auggie PromptReceived, got {other:?}"),
        }
    }

    #[test]
    fn parses_auggie_task_label_events() {
        let label_event = parse_iterm2(&[b"1337", b"AuggieTaskLabel=Ship JSON5 support"]);
        match label_event {
            Some(MetadataEvent::TaskLabelChanged { provider, label }) => {
                assert_eq!(provider, AiProvider::Auggie);
                assert_eq!(label, "Ship JSON5 support");
            }
            other => panic!("expected Auggie task label event, got {other:?}"),
        }

        let clear_event = parse_iterm2(&[b"1337", b"AuggieTaskLabelCleared"]);
        assert_eq!(
            clear_event,
            Some(MetadataEvent::TaskLabelCleared { provider: AiProvider::Auggie })
        );
    }

    #[test]
    fn rejects_empty_prompt() {
        let event = parse_iterm2(&[b"1337", b"ClaudePrompt="]);
        assert!(event.is_none());
    }

    #[test]
    fn truncates_long_prompt() {
        let long_text = "x".repeat(300);
        let payload = format!("ClaudePrompt={long_text}");
        let event = parse_iterm2(&[b"1337", payload.as_bytes()]);
        match event {
            Some(MetadataEvent::PromptReceived { text, .. }) => {
                assert_eq!(text.len(), 256);
            }
            other => panic!("expected PromptReceived, got {other:?}"),
        }
    }

    #[test]
    fn parses_context_field_on_claude_state() {
        let event = parse_iterm2(&[b"1337", b"ClaudeState=processing", b"context=73"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::ClaudeCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.context, Some(73));
            }
            other => panic!("expected Claude processing state with context, got {other:?}"),
        }
    }

    #[test]
    fn parses_context_field_on_codex_state() {
        let event = parse_iterm2(&[b"1337", b"CodexState=processing", b"context=42"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::CodexCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.context, Some(42));
            }
            other => panic!("expected Codex processing state with context, got {other:?}"),
        }
    }

    #[test]
    fn ignores_invalid_context_value() {
        let event = parse_iterm2(&[b"1337", b"ClaudeState=processing", b"context=abc"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::ClaudeCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.context, None);
            }
            other => panic!("expected Claude processing state with no context, got {other:?}"),
        }
    }

    #[test]
    fn ignores_context_value_above_100() {
        let event = parse_iterm2(&[b"1337", b"ClaudeState=processing", b"context=200"]);

        match event {
            Some(MetadataEvent::AiStateChanged(ai_state)) => {
                assert_eq!(ai_state.provider, AiProvider::ClaudeCode);
                assert_eq!(ai_state.state, AiState::Processing);
                assert_eq!(ai_state.context, None);
            }
            other => panic!("expected Claude processing state with no context, got {other:?}"),
        }
    }

    #[test]
    fn parses_claude_context_only_event() {
        let event = parse_iterm2(&[b"1337", b"ClaudeContext=42"]);
        assert_eq!(
            event,
            Some(MetadataEvent::AiContextChanged { provider: AiProvider::ClaudeCode, context: 42 })
        );
    }

    #[test]
    fn parses_codex_context_only_event() {
        let event = parse_iterm2(&[b"1337", b"CodexContext=7"]);
        assert_eq!(
            event,
            Some(MetadataEvent::AiContextChanged { provider: AiProvider::CodexCode, context: 7 })
        );
    }

    #[test]
    fn parses_auggie_context_only_event() {
        let event = parse_iterm2(&[b"1337", b"AuggieContext=99"]);
        assert_eq!(
            event,
            Some(MetadataEvent::AiContextChanged { provider: AiProvider::Auggie, context: 99 })
        );
    }

    #[test]
    fn drops_context_only_event_above_100() {
        let event = parse_iterm2(&[b"1337", b"ClaudeContext=150"]);
        assert!(event.is_none(), "out-of-range context should not synthesize a state");
    }

    #[test]
    fn drops_context_only_event_with_non_numeric_value() {
        let event = parse_iterm2(&[b"1337", b"ClaudeContext=oops"]);
        assert!(event.is_none(), "non-numeric context should not synthesize a state");
    }

    #[test]
    fn drops_context_only_event_with_empty_value() {
        let event = parse_iterm2(&[b"1337", b"ClaudeContext="]);
        assert!(event.is_none());
    }
}
