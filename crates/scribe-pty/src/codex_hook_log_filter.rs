/// Result of filtering: either the original bytes (no hook log lines removed)
/// or a new buffer with matching lines suppressed.
pub enum CodexHookLogOutput<'a> {
    /// No standard hook log lines were removed and no bytes are pending.
    Unchanged(&'a [u8]),
    /// One or more bytes were withheld or removed.
    Filtered(Vec<u8>),
}

impl CodexHookLogOutput<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            CodexHookLogOutput::Unchanged(bytes) => bytes,
            CodexHookLogOutput::Filtered(bytes) => bytes,
        }
    }
}

/// Stateful filter that removes contiguous Codex hook log blocks.
///
/// Candidate lines are buffered only from the start of a line. ANSI styling is
/// preserved for non-matching lines and ignored for hook-line classification.
pub struct CodexHookLogFilter {
    pending: Option<PendingLine>,
    pending_hook_block: Option<PendingHookBlock>,
    passthrough_until_newline: bool,
    at_line_start: bool,
    blank_line_state: BlankLineState,
}

struct PendingLine {
    raw: Vec<u8>,
    visible: String,
    escape_state: EscapeState,
}

struct PendingHookBlock {
    raw: Vec<u8>,
}

#[derive(Clone, Copy)]
enum EscapeState {
    None,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Clone, Copy, Default)]
enum BlankLineState {
    #[default]
    Keep,
    Suppress,
}

impl BlankLineState {
    fn should_suppress(self) -> bool {
        matches!(self, Self::Suppress)
    }
}

impl CodexHookLogFilter {
    const MAX_PENDING_HOOK_BLOCK_BYTES: usize = 64 * 1024;

    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: None,
            pending_hook_block: None,
            passthrough_until_newline: false,
            at_line_start: true,
            blank_line_state: BlankLineState::Keep,
        }
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some() || self.pending_hook_block.is_some()
    }

    /// Filter `input`, suppressing complete Codex hook log blocks.
    ///
    /// Partial candidate lines are retained until a later `filter()` call or
    /// `flush()`.
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> CodexHookLogOutput<'a> {
        let mut out = Vec::with_capacity(input.len());
        let mut modified = self.has_pending();

        for &byte in input {
            modified |= self.process_byte(byte, &mut out);
        }

        if modified || self.has_pending() {
            CodexHookLogOutput::Filtered(out)
        } else {
            CodexHookLogOutput::Unchanged(input)
        }
    }

    /// Flush any bytes currently buffered as a partial candidate line or hook block.
    ///
    /// Returns `None` when no bytes are pending or when the buffered bytes form
    /// a complete hook-block trailer or the first blank spacer line after a
    /// hidden hook block.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.passthrough_until_newline = false;
        self.at_line_start = true;

        if let Some(mut pending_hook_block) = self.pending_hook_block.take() {
            if let Some(pending) = self.pending.take() {
                pending_hook_block.raw.extend_from_slice(&pending.raw);
            }
            self.blank_line_state = BlankLineState::Keep;
            return Some(pending_hook_block.raw);
        }

        let pending = self.pending.take()?;
        let trimmed = pending.visible.trim();
        if is_hook_block_end_line(trimmed)
            || (self.blank_line_state.should_suppress() && trimmed.is_empty())
        {
            self.blank_line_state = BlankLineState::Keep;
            return None;
        }

        self.blank_line_state = BlankLineState::Keep;
        Some(pending.raw)
    }

    fn process_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        if self.passthrough_until_newline {
            out.push(byte);
            if byte == b'\n' {
                self.passthrough_until_newline = false;
                self.at_line_start = true;
            } else {
                self.at_line_start = false;
            }
            return false;
        }

        if let Some(mut pending) = self.pending.take() {
            return self.process_pending_byte(&mut pending, byte, out);
        }

        if self.pending_hook_block.is_some() && self.at_line_start {
            let mut pending = PendingLine::new();
            pending.raw.push(byte);
            pending.record_visible(byte);
            if byte == b'\n' {
                return self.finish_pending_line(&pending, out);
            }
            self.pending = Some(pending);
            return false;
        }

        if self.blank_line_state.should_suppress() && self.at_line_start {
            let mut pending = PendingLine::new();
            pending.raw.push(byte);
            pending.record_visible(byte);
            if byte == b'\n' {
                return self.finish_pending_line(&pending, out);
            }
            self.pending = Some(pending);
            return false;
        }

        if self.at_line_start && can_begin_candidate(byte) {
            let mut pending = PendingLine::new();
            pending.raw.push(byte);
            pending.record_visible(byte);
            self.pending = Some(pending);
            return false;
        }

        out.push(byte);
        self.at_line_start = byte == b'\n';
        false
    }

    fn process_pending_byte(
        &mut self,
        pending: &mut PendingLine,
        byte: u8,
        out: &mut Vec<u8>,
    ) -> bool {
        pending.raw.push(byte);
        pending.record_visible(byte);

        if byte == b'\n' {
            return self.finish_pending_line(pending, out);
        }

        if self.pending_hook_block.is_some() {
            self.pending = Some(std::mem::take(pending));
            return false;
        }

        let trimmed = pending.visible.trim_start();
        if self.blank_line_state.should_suppress() && trimmed.is_empty() {
            self.pending = Some(std::mem::take(pending));
            return false;
        }

        if is_standard_hook_line_prefix(trimmed) {
            self.pending = Some(std::mem::take(pending));
            return false;
        }

        out.extend_from_slice(&pending.raw);
        self.passthrough_until_newline = true;
        self.at_line_start = false;
        false
    }

    fn finish_pending_line(&mut self, pending: &PendingLine, out: &mut Vec<u8>) -> bool {
        let trimmed = pending.visible.trim();
        let suppress_blank_line = self.blank_line_state.should_suppress() && trimmed.is_empty();
        self.at_line_start = true;

        if self.pending_hook_block.is_some() {
            return self.finish_pending_hook_block_line(pending, trimmed, out);
        }

        if is_hook_block_start_line(trimmed) {
            let mut pending_hook_block = PendingHookBlock::new();
            pending_hook_block.raw.extend_from_slice(&pending.raw);
            self.pending_hook_block = Some(pending_hook_block);
            self.blank_line_state = BlankLineState::Keep;
            return true;
        }
        if is_hook_block_end_line(trimmed) {
            self.blank_line_state = BlankLineState::Suppress;
            return true;
        }
        if suppress_blank_line {
            self.blank_line_state = BlankLineState::Keep;
            return true;
        }

        self.blank_line_state = BlankLineState::Keep;
        if !pending.raw.is_empty() {
            out.extend_from_slice(&pending.raw);
        }
        false
    }

    fn finish_pending_hook_block_line(
        &mut self,
        pending: &PendingLine,
        trimmed: &str,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some(active_hook_block) = self.pending_hook_block.as_mut() else {
            return false;
        };
        active_hook_block.raw.extend_from_slice(&pending.raw);
        let should_fail_open = active_hook_block.raw.len() > Self::MAX_PENDING_HOOK_BLOCK_BYTES;

        if is_hook_block_end_line(trimmed) {
            self.pending_hook_block = None;
            self.blank_line_state = BlankLineState::Suppress;
            return true;
        }

        if let Some(flushed_hook_block) =
            should_fail_open.then(|| self.pending_hook_block.take()).flatten()
        {
            self.blank_line_state = BlankLineState::Keep;
            out.extend_from_slice(&flushed_hook_block.raw);
        }

        true
    }
}

impl Default for CodexHookLogFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingLine {
    fn new() -> Self {
        Self { raw: Vec::new(), visible: String::new(), escape_state: EscapeState::None }
    }

    fn record_visible(&mut self, byte: u8) {
        match self.escape_state {
            EscapeState::None => match byte {
                0x1B => self.escape_state = EscapeState::Esc,
                b'\r' | b'\n' => {}
                _ if byte.is_ascii() => self.visible.push(char::from(byte)),
                _ => {}
            },
            EscapeState::Esc => {
                self.escape_state = match byte {
                    b'[' => EscapeState::Csi,
                    b']' => EscapeState::Osc,
                    _ => EscapeState::None,
                };
            }
            EscapeState::Csi => {
                if matches!(byte, 0x40..=0x7E) {
                    self.escape_state = EscapeState::None;
                }
            }
            EscapeState::Osc => match byte {
                0x07 => self.escape_state = EscapeState::None,
                0x1B => self.escape_state = EscapeState::OscEsc,
                _ => {}
            },
            EscapeState::OscEsc => {
                self.escape_state =
                    if byte == b'\\' { EscapeState::None } else { EscapeState::Osc };
            }
        }
    }
}

impl PendingHookBlock {
    fn new() -> Self {
        Self { raw: Vec::new() }
    }
}

impl Default for PendingLine {
    fn default() -> Self {
        Self::new()
    }
}

fn can_begin_candidate(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b' ' | b'\t' | 0x1B) || !byte.is_ascii()
}

const CODEX_HOOK_EVENT_NAMES: &[&str] =
    &["PreToolUse", "PostToolUse", "SessionStart", "UserPromptSubmit", "Stop"];

const CODEX_HOOK_END_SUFFIXES: &[&str] =
    &[" hook (completed)", " hook (failed)", " hook (blocked)", " hook (stopped)"];

fn is_standard_hook_line_prefix(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    running_hook_line_prefix(stripped) || completed_hook_line_prefix(stripped)
}

fn is_hook_block_start_line(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    running_hook_line(stripped)
}

fn is_hook_block_end_line(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    completed_hook_line(stripped)
}

fn running_hook_line_prefix(s: &str) -> bool {
    let prefix = "Running ";
    if prefix.starts_with(s) {
        return true;
    }

    s.strip_prefix(prefix)
        .is_some_and(|rest| known_hook_name_then_prefix(rest, hook_start_suffix_prefix))
}

fn running_hook_line(s: &str) -> bool {
    s.strip_prefix("Running ").and_then(strip_known_hook_name).is_some_and(hook_start_suffix)
}

fn completed_hook_line_prefix(s: &str) -> bool {
    known_hook_name_then_prefix(s, hook_end_suffix_prefix)
}

fn completed_hook_line(s: &str) -> bool {
    strip_known_hook_name(s).is_some_and(hook_end_suffix)
}

fn known_hook_name_then_prefix(s: &str, suffix_prefix: impl Fn(&str) -> bool) -> bool {
    CODEX_HOOK_EVENT_NAMES.iter().any(|hook_name| {
        hook_name.starts_with(s) || s.strip_prefix(hook_name).is_some_and(&suffix_prefix)
    })
}

fn strip_known_hook_name(s: &str) -> Option<&str> {
    CODEX_HOOK_EVENT_NAMES.iter().find_map(|hook_name| s.strip_prefix(hook_name))
}

fn hook_start_suffix_prefix(s: &str) -> bool {
    " hook".starts_with(s) || s.starts_with(" hook:")
}

fn hook_start_suffix(s: &str) -> bool {
    s == " hook" || s.starts_with(" hook:")
}

fn hook_end_suffix_prefix(s: &str) -> bool {
    CODEX_HOOK_END_SUFFIXES.iter().any(|suffix| suffix.starts_with(s))
}

fn hook_end_suffix(s: &str) -> bool {
    CODEX_HOOK_END_SUFFIXES.contains(&s)
}

fn strip_leading_hook_log_decorators(s: &str) -> Option<&str> {
    let s = s.trim_start_matches([' ', '\t']);
    let Some(first) = s.chars().next() else {
        return Some("");
    };

    if !matches!(first, '•' | '·' | '◦') {
        return Some(s);
    }

    let rest = &s[first.len_utf8()..];
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim_start_matches([' ', '\t']))
}
