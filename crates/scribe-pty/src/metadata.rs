use std::path::PathBuf;

use scribe_common::ai_state::{AiProcessState, AiProvider};
use scribe_common::protocol::{PromptMarkKind, SessionContext};

/// Maximum length for window title strings (chars). Longer titles are truncated.
const MAX_TITLE_LEN: usize = 4096;

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

    /// Parse OSC 1337 iTerm2-extension payloads.
    ///
    /// Scribe used to recognize `<Provider>State=…`, `<Provider>Prompt=…`,
    /// `<Provider>TaskLabel=…`, `<Provider>Context=…`, and the legacy
    /// `AiState=…` formats here. Those AI-hook-originated payloads now arrive
    /// over the structured hook channel (see `scribe-common::hook`); the
    /// only OSC 1337 payloads still parsed are emitted from a shell with a
    /// real controlling TTY: `ScribeContext` (session context) and
    /// `ScribeAiLaunch=` (the pre-arm sentinel, FR-023).
    fn parse_iterm2(params: &[&[u8]]) -> Option<MetadataEvent> {
        let payload_bytes = params.get(1)?;
        let payload = String::from_utf8_lossy(payload_bytes);

        if payload == "ScribeContext" || payload.starts_with("ScribeContext=") {
            return Some(Self::parse_session_context(payload.as_ref(), params));
        }

        if let Some(provider_id) = payload.strip_prefix("ScribeAiLaunch=")
            && let Some(provider) = AiProvider::from_id(provider_id.trim())
        {
            return Some(MetadataEvent::AiProviderArmed { provider });
        }

        None
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
    use scribe_common::ai_state::AiProvider;

    fn parse_iterm2(payload: &[&[u8]]) -> Option<MetadataEvent> {
        MetadataParser::process_osc(payload)
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
    fn parses_pre_arm_sentinel_for_each_provider() {
        // FR-023: pre-arm sentinel OSC parsing is retained because it is
        // emitted from a real shell preexec with a controlling TTY, not
        // from a hook subprocess.
        let claude = parse_iterm2(&[b"1337", b"ScribeAiLaunch=claude_code"]);
        assert_eq!(
            claude,
            Some(MetadataEvent::AiProviderArmed { provider: AiProvider::ClaudeCode })
        );

        let codex = parse_iterm2(&[b"1337", b"ScribeAiLaunch=codex_code"]);
        assert_eq!(codex, Some(MetadataEvent::AiProviderArmed { provider: AiProvider::CodexCode }));

        let auggie = parse_iterm2(&[b"1337", b"ScribeAiLaunch=auggie"]);
        assert_eq!(auggie, Some(MetadataEvent::AiProviderArmed { provider: AiProvider::Auggie }));
    }

    #[test]
    fn ignores_pre_arm_sentinel_with_unknown_provider() {
        let event = parse_iterm2(&[b"1337", b"ScribeAiLaunch=unknown_provider"]);
        assert!(event.is_none());
    }

    #[test]
    fn ai_hook_osc_payloads_are_no_longer_parsed() {
        // Per FR-022, all AI-hook-originated OSC 1337 payloads must be
        // unrecognized by the metadata parser. These are now delivered via
        // the structured hook channel (`scribe-common::hook`). Each call
        // below would have produced a `MetadataEvent` under the old parser.
        assert!(parse_iterm2(&[b"1337", b"ClaudeState=processing"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"CodexState=processing", b"tool=Bash"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"AuggieState=waiting_for_input"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"ClaudeState=inactive"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"ClaudePrompt=Fix the login bug"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"CodexPrompt=Add OAuth support"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"AuggieTaskLabel=Ship JSON5"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"AuggieTaskLabelCleared"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"ClaudeContext=42"]).is_none());
        assert!(parse_iterm2(&[b"1337", b"AiState=state=processing"]).is_none());
    }
}
