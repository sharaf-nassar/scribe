use std::path::PathBuf;

use scribe_common::ai_state::{AiProcessState, AiState};
use scribe_common::ids::SessionId;

/// Maximum length for window title strings (chars). Longer titles are truncated.
const MAX_TITLE_LEN: usize = 4096;

/// Maximum length for AI metadata fields (tool, agent, model) in chars.
const MAX_AI_FIELD_LEN: usize = 256;

/// Events extracted from the PTY output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataEvent {
    CwdChanged(PathBuf),
    TitleChanged(String),
    AiStateChanged(AiProcessState),
    /// The AI state was explicitly cleared (OSC 1337 `ClaudeState=inactive`).
    AiStateCleared,
    Bell,
}

/// Stateful parser that extracts OSC metadata from a VTE Perform implementation.
pub struct MetadataParser {
    session_id: SessionId,
}

impl MetadataParser {
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Process an OSC sequence and return a metadata event if one was extracted.
    /// The `params` slice contains the semicolon-delimited parts.
    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "Consistent API: process_* methods take &self for future extensibility"
    )]
    pub fn process_osc(&self, params: &[&[u8]]) -> Option<MetadataEvent> {
        let osc_number = params.first()?;

        match *osc_number {
            b"0" | b"2" => Self::parse_title(params),
            b"7" => Self::parse_cwd(params),
            b"1337" => Self::parse_iterm2(params),
            _ => None,
        }
    }

    /// Process a C0 control byte and return a metadata event if applicable.
    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "Consistent API: process_* methods take &self for future extensibility"
    )]
    pub fn process_execute(&self, byte: u8) -> Option<MetadataEvent> {
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

    fn parse_iterm2(params: &[&[u8]]) -> Option<MetadataEvent> {
        let payload_bytes = params.get(1)?;
        let payload = String::from_utf8_lossy(payload_bytes);

        // Primary format: ESC ] 1337 ; ClaudeState=<state> [; key=value ...] ST
        if let Some(state_value) = payload.strip_prefix("ClaudeState=") {
            return Self::parse_claude_state(state_value, params);
        }

        // Legacy format: ESC ] 1337 ; AiState=state=<state>;key=val... ST
        // Kept for backwards compatibility with older Claude Code versions.
        if let Some(legacy_payload) = payload.strip_prefix("AiState=") {
            return Self::parse_legacy_ai_state(legacy_payload);
        }

        None
    }

    /// Parse the legacy `AiState=state=X;key=val` single-payload format.
    fn parse_legacy_ai_state(payload: &str) -> Option<MetadataEvent> {
        let mut builder = AiStateBuilder::default();
        for part in payload.split(';') {
            if let Some((key, value)) = part.split_once('=') {
                builder.apply(key, value);
            }
        }
        builder.build()
    }

    fn parse_claude_state(state_value: &str, params: &[&[u8]]) -> Option<MetadataEvent> {
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

        let mut builder = AiStateBuilder { state: Some(state), ..AiStateBuilder::default() };

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
}

/// Accumulates key=value fields from OSC 1337 `ClaudeState` params.
#[derive(Default)]
struct AiStateBuilder {
    state: Option<AiState>,
    tool: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    context: Option<u8>,
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
            "context" => self.context = value.parse().ok(),
            _ => {} // Ignore unknown keys (forward compatibility)
        }
    }

    fn build(self) -> Option<MetadataEvent> {
        let state = self.state?;
        Some(MetadataEvent::AiStateChanged(AiProcessState {
            state,
            tool: self.tool,
            agent: self.agent,
            model: self.model,
            context: self.context,
        }))
    }
}

/// Truncate a string to at most `max_chars` Unicode characters.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
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
