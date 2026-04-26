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
    sync_start_match_len: usize,
    sync_start_buffer: Vec<u8>,
    pending_sync_block: Option<PendingSyncBlock>,
}

struct PendingLine {
    raw: Vec<u8>,
    visible: String,
    escape_state: EscapeState,
    utf8_visible_bytes: Vec<u8>,
    complete_hook_line_raw_len: Option<usize>,
}

struct PendingHookBlock {
    raw: Vec<u8>,
}

struct PendingSyncBlock {
    raw: Vec<u8>,
    end_match_len: usize,
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
    const MAX_PENDING_LINE_PREFIX_BYTES: usize = 1024;
    const SYNC_UPDATE_START: &[u8] = b"\x1b[?2026h";
    const SYNC_UPDATE_END: &[u8] = b"\x1b[?2026l";
    const SYNC_UPDATE_START_FIRST: u8 = 0x1B;

    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: None,
            pending_hook_block: None,
            passthrough_until_newline: false,
            at_line_start: true,
            blank_line_state: BlankLineState::Keep,
            sync_start_match_len: 0,
            sync_start_buffer: Vec::new(),
            pending_sync_block: None,
        }
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
            || self.pending_hook_block.is_some()
            || self.sync_start_match_len > 0
            || !self.sync_start_buffer.is_empty()
            || self.pending_sync_block.is_some()
    }

    /// Filter `input`, suppressing complete Codex hook log blocks.
    ///
    /// Partial candidate lines are retained until a later `filter()` call or
    /// `flush()`.
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> CodexHookLogOutput<'a> {
        let mut out = Vec::with_capacity(input.len());
        let mut modified = self.has_pending();

        for &byte in input {
            modified |= self.process_stream_byte(byte, &mut out);
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
        let mut out = Vec::new();

        if let Some(pending_sync_block) = self.pending_sync_block.take() {
            out.extend_from_slice(&pending_sync_block.raw);
        }

        if !self.sync_start_buffer.is_empty() {
            let buffered = std::mem::take(&mut self.sync_start_buffer);
            self.sync_start_match_len = 0;
            for byte in buffered {
                self.process_byte(byte, &mut out);
            }
        }

        if let Some(flushed_line_state) = self.flush_line_state() {
            out.extend_from_slice(&flushed_line_state);
        }

        (!out.is_empty()).then_some(out)
    }

    fn flush_line_state(&mut self) -> Option<Vec<u8>> {
        self.passthrough_until_newline = false;
        self.at_line_start = true;

        if let Some(mut pending_hook_block) = self.pending_hook_block.take() {
            let pending = self.pending.take();
            return self.flush_pending_hook_block(&mut pending_hook_block, pending);
        }

        let pending = self.pending.take()?;
        if let Some(trailing_raw) = self.hidden_hook_trailing_raw(&pending) {
            return (!trailing_raw.is_empty()).then_some(trailing_raw);
        }
        let trimmed = pending.visible.trim();
        if is_hook_block_end_line(trimmed)
            || (self.blank_line_state.should_suppress() && pending.is_raw_blank_spacer())
        {
            self.blank_line_state = BlankLineState::Keep;
            return None;
        }

        self.blank_line_state = BlankLineState::Keep;
        Some(pending.raw)
    }

    fn flush_pending_hook_block(
        &mut self,
        pending_hook_block: &mut PendingHookBlock,
        pending: Option<PendingLine>,
    ) -> Option<Vec<u8>> {
        if let Some(trailing_raw) =
            pending.as_ref().and_then(|line| self.hidden_hook_trailing_raw(line))
        {
            return (!trailing_raw.is_empty()).then_some(trailing_raw);
        }

        if let Some(pending) = pending {
            pending_hook_block.raw.extend_from_slice(&pending.raw);
        }

        self.blank_line_state = BlankLineState::Keep;
        Some(std::mem::take(&mut pending_hook_block.raw))
    }

    fn hidden_hook_trailing_raw(&mut self, pending: &PendingLine) -> Option<Vec<u8>> {
        let (line, trailing_raw) = pending.split_complete_hook_line_prefix()?;
        if !is_hook_block_end_line(line.visible.trim()) {
            return None;
        }

        self.blank_line_state = BlankLineState::Keep;
        Some(trailing_raw)
    }

    fn process_stream_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        if let Some(sync_block) = self.pending_sync_block.as_mut() {
            sync_block.raw.push(byte);

            let end_seq = Self::SYNC_UPDATE_END;
            sync_block.end_match_len = update_match_len(sync_block.end_match_len, byte, end_seq);
            if sync_block.end_match_len == end_seq.len() {
                return self.finish_pending_sync_block(out);
            }

            return true;
        }

        if self.sync_start_match_len > 0 || byte == Self::SYNC_UPDATE_START_FIRST {
            return self.process_sync_start_candidate(byte, out);
        }

        self.process_byte(byte, out)
    }

    fn finish_pending_sync_block(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(completed_sync_block) = self.pending_sync_block.take() else {
            return false;
        };

        let transformed = transform_sync_block(&completed_sync_block.raw);
        out.extend_from_slice(&transformed);
        transformed != completed_sync_block.raw
    }

    fn process_sync_start_candidate(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        let start_seq = Self::SYNC_UPDATE_START;
        let Some(expected) = start_seq.get(self.sync_start_match_len).copied() else {
            self.sync_start_match_len = 0;
            self.sync_start_buffer.clear();
            return self.process_byte(byte, out);
        };

        if byte == expected {
            self.sync_start_buffer.push(byte);
            self.sync_start_match_len += 1;

            if self.sync_start_match_len == start_seq.len() {
                self.begin_pending_sync_block(out);
            }

            return true;
        }

        let mut modified = false;
        let buffered = std::mem::take(&mut self.sync_start_buffer);
        self.sync_start_match_len = 0;
        for buffered_byte in buffered {
            modified |= self.process_byte(buffered_byte, out);
        }

        if byte == Self::SYNC_UPDATE_START_FIRST {
            self.sync_start_buffer.push(byte);
            self.sync_start_match_len = 1;
            return true;
        }

        modified | self.process_byte(byte, out)
    }

    fn begin_pending_sync_block(&mut self, out: &mut Vec<u8>) {
        if let Some(flushed_line_state) = self.flush_line_state() {
            out.extend_from_slice(&flushed_line_state);
        }
        let raw = std::mem::take(&mut self.sync_start_buffer);
        self.sync_start_match_len = 0;
        self.pending_sync_block = Some(PendingSyncBlock { raw, end_match_len: 0 });
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
        if pending.should_split_before_byte(byte) {
            return self.finish_pending_at_implicit_boundary(pending, byte, out);
        }

        pending.raw.push(byte);
        pending.record_visible(byte);

        if byte == b'\n' {
            return self.finish_pending_line(pending, out);
        }

        if self.pending_hook_block.is_some() {
            self.pending = Some(std::mem::take(pending));
            return false;
        }

        if !pending.utf8_visible_bytes.is_empty() {
            self.pending = Some(std::mem::take(pending));
            return false;
        }

        let trimmed = pending.visible.trim_start();
        if trimmed.is_empty() && pending.raw.len() <= Self::MAX_PENDING_LINE_PREFIX_BYTES {
            self.pending = Some(std::mem::take(pending));
            return false;
        }
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

    fn finish_pending_at_implicit_boundary(
        &mut self,
        pending: &PendingLine,
        next_byte: u8,
        out: &mut Vec<u8>,
    ) -> bool {
        let Some((line, trailing_raw)) = pending.split_complete_hook_line_prefix() else {
            let mut modified = self.finish_pending_line(pending, out);
            modified |= self.process_byte(next_byte, out);
            return modified;
        };

        let mut modified = self.finish_pending_line(&line, out);
        for trailer_byte in trailing_raw {
            modified |= self.process_byte(trailer_byte, out);
        }
        modified |= self.process_byte(next_byte, out);
        modified
    }

    fn finish_pending_line(&mut self, pending: &PendingLine, out: &mut Vec<u8>) -> bool {
        let trimmed = pending.visible.trim();
        let suppress_blank_line =
            self.blank_line_state.should_suppress() && pending.is_raw_blank_spacer();
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
        Self {
            raw: Vec::new(),
            visible: String::new(),
            escape_state: EscapeState::None,
            utf8_visible_bytes: Vec::new(),
            complete_hook_line_raw_len: None,
        }
    }

    fn record_visible(&mut self, byte: u8) {
        let mut appended_visible = false;
        match self.escape_state {
            EscapeState::None => match byte {
                0x1B => {
                    self.utf8_visible_bytes.clear();
                    self.escape_state = EscapeState::Esc;
                }
                b'\r' | b'\n' => self.utf8_visible_bytes.clear(),
                _ if byte.is_ascii() => {
                    self.utf8_visible_bytes.clear();
                    self.visible.push(char::from(byte));
                    appended_visible = true;
                }
                _ => appended_visible = self.record_utf8_visible(byte),
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

        if appended_visible
            && self.complete_hook_line_raw_len.is_none()
            && is_hook_block_end_line(self.visible.trim())
        {
            self.complete_hook_line_raw_len = Some(self.raw.len());
        }
    }

    fn record_utf8_visible(&mut self, byte: u8) -> bool {
        self.utf8_visible_bytes.push(byte);
        match std::str::from_utf8(&self.utf8_visible_bytes) {
            Ok(s) => {
                self.visible.push_str(s);
                self.utf8_visible_bytes.clear();
                true
            }
            Err(err) if err.error_len().is_none() && self.utf8_visible_bytes.len() < 4 => false,
            Err(_) => {
                self.utf8_visible_bytes.clear();
                false
            }
        }
    }

    fn is_raw_blank_spacer(&self) -> bool {
        self.raw.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    }

    fn should_split_before_byte(&self, byte: u8) -> bool {
        self.complete_hook_line_raw_len
            .is_some_and(|raw_len| raw_len < self.raw.len() && self.byte_begins_visible(byte))
    }

    fn byte_begins_visible(&self, byte: u8) -> bool {
        matches!(self.escape_state, EscapeState::None) && !matches!(byte, 0x1B | b'\r' | b'\n')
    }

    fn split_complete_hook_line_prefix(&self) -> Option<(Self, Vec<u8>)> {
        let raw_len = self.complete_hook_line_raw_len?;
        if raw_len >= self.raw.len() {
            return None;
        }

        let line_raw = self.raw.get(..raw_len)?;
        let trailing_suffix = self.raw.get(raw_len..)?;
        let line = pending_line_from_raw(line_raw);
        if !complete_hook_line(line.visible.trim()) {
            return None;
        }

        let mut trailing_raw = sgr_restore_bytes_for_prefix(line_raw);
        trailing_raw.extend_from_slice(trailing_suffix);
        Some((line, trailing_raw))
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
    &["PreToolUse", "PermissionRequest", "PostToolUse", "SessionStart", "UserPromptSubmit", "Stop"];

const CODEX_HOOK_END_SUFFIXES: &[&str] =
    &[" hook (completed)", " hook (failed)", " hook (blocked)", " hook (stopped)"];

const CODEX_HOOK_LABEL_PREFIX: &str = "hook:";

const CODEX_HOOK_LABEL_END_SUFFIXES: &[&str] = &[" Completed", " Failed", " Blocked", " Stopped"];

fn is_standard_hook_line_prefix(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    if stripped.is_empty() {
        return has_only_hook_log_decorator_prefix(s);
    }
    running_hook_line_prefix(stripped)
        || completed_hook_line_prefix(stripped)
        || labeled_hook_line_prefix(stripped)
}

fn is_hook_block_start_line(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    running_hook_line(stripped) || labeled_hook_start_line(stripped)
}

fn is_hook_block_end_line(s: &str) -> bool {
    let Some(stripped) = strip_leading_hook_log_decorators(s) else {
        return false;
    };
    completed_hook_line(stripped) || labeled_hook_end_line(stripped)
}

fn complete_hook_line(s: &str) -> bool {
    is_hook_block_start_line(s) || is_hook_block_end_line(s)
}

fn running_hook_line_prefix(s: &str) -> bool {
    let prefix = "Running ";
    if !s.is_empty() && prefix.starts_with(s) {
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

fn labeled_hook_line_prefix(s: &str) -> bool {
    if !s.is_empty() && CODEX_HOOK_LABEL_PREFIX.starts_with(s) {
        return true;
    }

    let Some(rest) = s.strip_prefix(CODEX_HOOK_LABEL_PREFIX) else {
        return false;
    };
    let rest = rest.trim_start_matches([' ', '\t']);
    rest.is_empty() || known_hook_name_then_prefix(rest, hook_label_end_suffix_prefix)
}

fn labeled_hook_start_line(s: &str) -> bool {
    s.strip_prefix(CODEX_HOOK_LABEL_PREFIX)
        .map(str::trim_start)
        .and_then(strip_known_hook_name)
        .is_some_and(str::is_empty)
}

fn labeled_hook_end_line(s: &str) -> bool {
    s.strip_prefix(CODEX_HOOK_LABEL_PREFIX)
        .map(str::trim_start)
        .and_then(strip_known_hook_name)
        .is_some_and(hook_label_end_suffix)
}

fn known_hook_name_then_prefix(s: &str, suffix_prefix: impl Fn(&str) -> bool) -> bool {
    CODEX_HOOK_EVENT_NAMES.iter().any(|hook_name| {
        (!s.is_empty() && hook_name.starts_with(s))
            || s.strip_prefix(hook_name).is_some_and(&suffix_prefix)
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

fn hook_label_end_suffix_prefix(s: &str) -> bool {
    CODEX_HOOK_LABEL_END_SUFFIXES.iter().any(|suffix| suffix.starts_with(s))
}

fn hook_label_end_suffix(s: &str) -> bool {
    CODEX_HOOK_LABEL_END_SUFFIXES.contains(&s)
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

fn has_only_hook_log_decorator_prefix(s: &str) -> bool {
    let s = s.trim_start_matches([' ', '\t']);
    let Some(first) = s.chars().next() else {
        return false;
    };

    if !matches!(first, '•' | '·' | '◦') {
        return false;
    }

    s[first.len_utf8()..].chars().all(|c| matches!(c, ' ' | '\t'))
}

fn update_match_len(current: usize, byte: u8, needle: &[u8]) -> usize {
    if needle.get(current).is_some_and(|expected| *expected == byte) {
        current + 1
    } else {
        usize::from(needle.first().is_some_and(|expected| *expected == byte))
    }
}

fn transform_sync_block(block: &[u8]) -> Vec<u8> {
    let Some(body) = block
        .strip_prefix(CodexHookLogFilter::SYNC_UPDATE_START)
        .and_then(|rest| rest.strip_suffix(CodexHookLogFilter::SYNC_UPDATE_END))
    else {
        return block.to_vec();
    };

    let lines = split_sync_block_lines(body);
    let analyses: Vec<_> = lines.into_iter().map(analyze_sync_block_line).collect();
    if !analyses.iter().any(SyncBlockLineAnalysis::has_hook) {
        return block.to_vec();
    }

    let Some(last_kept_line_idx) =
        analyses.iter().rposition(SyncBlockLineAnalysis::contributes_content)
    else {
        let mut kept_body = Vec::new();
        for analysis in &analyses {
            analysis.extend_partial_bytes(&mut kept_body);
        }

        if kept_body.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(
            CodexHookLogFilter::SYNC_UPDATE_START.len()
                + kept_body.len()
                + CodexHookLogFilter::SYNC_UPDATE_END.len(),
        );
        out.extend_from_slice(CodexHookLogFilter::SYNC_UPDATE_START);
        out.extend_from_slice(&kept_body);
        out.extend_from_slice(CodexHookLogFilter::SYNC_UPDATE_END);
        return out;
    };

    let (kept_analyses, trailing_analyses) = analyses.split_at(last_kept_line_idx + 1);
    if trailing_analyses.iter().any(|line| !line.is_hookish()) {
        return block.to_vec();
    }

    let kept_body_len = kept_analyses.iter().map(SyncBlockLineAnalysis::kept_len).sum::<usize>();
    let mut out = Vec::with_capacity(
        CodexHookLogFilter::SYNC_UPDATE_START.len()
            + kept_body_len
            + CodexHookLogFilter::SYNC_UPDATE_END.len(),
    );
    out.extend_from_slice(CodexHookLogFilter::SYNC_UPDATE_START);
    for analysis in kept_analyses {
        analysis.extend_kept_bytes(&mut out);
    }
    out.extend_from_slice(CodexHookLogFilter::SYNC_UPDATE_END);
    out
}

#[derive(Clone, Copy)]
struct SyncBlockLine<'a> {
    raw: &'a [u8],
}

struct SyncBlockLineAnalysis<'a> {
    line: SyncBlockLine<'a>,
    retention: SyncBlockRetention,
    kind: SyncBlockLineKind,
}

enum SyncBlockRetention {
    Whole,
    Partial(Vec<u8>),
    Drop,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncBlockLineKind {
    Content,
    Hook,
    Spacer,
    HookPrefixContent,
}

impl SyncBlockLineAnalysis<'_> {
    fn contributes_content(&self) -> bool {
        matches!(self.kind, SyncBlockLineKind::Content | SyncBlockLineKind::HookPrefixContent)
    }

    fn has_hook(&self) -> bool {
        matches!(self.kind, SyncBlockLineKind::Hook | SyncBlockLineKind::HookPrefixContent)
    }

    fn is_hookish(&self) -> bool {
        matches!(self.kind, SyncBlockLineKind::Hook | SyncBlockLineKind::Spacer)
    }

    fn kept_len(&self) -> usize {
        match &self.retention {
            SyncBlockRetention::Whole => self.line.raw.len(),
            SyncBlockRetention::Partial(kept_bytes) => kept_bytes.len(),
            SyncBlockRetention::Drop => 0,
        }
    }

    fn extend_kept_bytes(&self, out: &mut Vec<u8>) {
        match &self.retention {
            SyncBlockRetention::Whole => out.extend_from_slice(self.line.raw),
            SyncBlockRetention::Partial(kept_bytes) => out.extend_from_slice(kept_bytes),
            SyncBlockRetention::Drop => {}
        }
    }

    fn extend_partial_bytes(&self, out: &mut Vec<u8>) {
        if let SyncBlockRetention::Partial(kept_bytes) = &self.retention {
            out.extend_from_slice(kept_bytes);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SgrColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Default)]
struct SgrStyleState {
    fg: Option<SgrColor>,
    bg: Option<SgrColor>,
    flags: u8,
}

impl SgrStyleState {
    const FLAG_BOLD: u8 = 1 << 0;
    const FLAG_DIM: u8 = 1 << 1;
    const FLAG_ITALIC: u8 = 1 << 2;
    const FLAG_UNDERLINE: u8 = 1 << 3;
    const FLAG_INVERSE: u8 = 1 << 4;
    const FLAG_HIDDEN: u8 = 1 << 5;
    const FLAG_STRIKETHROUGH: u8 = 1 << 6;

    fn is_default(self) -> bool {
        self.fg.is_none() && self.bg.is_none() && self.flags == 0
    }

    fn has_flag(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    fn set_flag(&mut self, flag: u8, enabled: bool) {
        if enabled {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }

    fn into_restore_bytes(self) -> Vec<u8> {
        if self.is_default() {
            return Vec::new();
        }

        let mut params: Vec<String> = vec![String::from("0")];
        if self.has_flag(Self::FLAG_BOLD) {
            params.push(String::from("1"));
        }
        if self.has_flag(Self::FLAG_DIM) {
            params.push(String::from("2"));
        }
        if self.has_flag(Self::FLAG_ITALIC) {
            params.push(String::from("3"));
        }
        if self.has_flag(Self::FLAG_UNDERLINE) {
            params.push(String::from("4"));
        }
        if self.has_flag(Self::FLAG_INVERSE) {
            params.push(String::from("7"));
        }
        if self.has_flag(Self::FLAG_HIDDEN) {
            params.push(String::from("8"));
        }
        if self.has_flag(Self::FLAG_STRIKETHROUGH) {
            params.push(String::from("9"));
        }
        if let Some(fg) = self.fg {
            push_color_params(&mut params, fg, false);
        }
        if let Some(bg) = self.bg {
            push_color_params(&mut params, bg, true);
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(params.join(";").as_bytes());
        out.push(b'm');
        out
    }
}

fn split_sync_block_lines(body: &[u8]) -> Vec<SyncBlockLine<'_>> {
    body.split_inclusive(|&byte| byte == b'\n').map(|raw| SyncBlockLine { raw }).collect()
}

fn analyze_sync_block_line(line: SyncBlockLine<'_>) -> SyncBlockLineAnalysis<'_> {
    if let Some((keep_tail, trailing_raw)) = split_sync_block_hook_line_prefix(line.raw) {
        return SyncBlockLineAnalysis {
            line,
            retention: if keep_tail {
                SyncBlockRetention::Partial(trailing_raw)
            } else {
                SyncBlockRetention::Drop
            },
            kind: if keep_tail {
                SyncBlockLineKind::HookPrefixContent
            } else {
                SyncBlockLineKind::Hook
            },
        };
    }

    let pending = pending_line_from_raw(line.raw);
    let trimmed = pending.visible.trim();
    if trimmed.is_empty() {
        let is_raw_blank_spacer = pending.is_raw_blank_spacer();
        return SyncBlockLineAnalysis {
            line,
            retention: SyncBlockRetention::Whole,
            kind: if is_raw_blank_spacer {
                SyncBlockLineKind::Spacer
            } else {
                SyncBlockLineKind::Content
            },
        };
    }

    let is_hook = is_hook_visible_line(trimmed);
    SyncBlockLineAnalysis {
        line,
        retention: if is_hook { SyncBlockRetention::Drop } else { SyncBlockRetention::Whole },
        kind: if is_hook { SyncBlockLineKind::Hook } else { SyncBlockLineKind::Content },
    }
}

fn push_color_params(params: &mut Vec<String>, color: SgrColor, background: bool) {
    let prefix = if background { "48" } else { "38" };
    match color {
        SgrColor::Indexed(idx) => {
            params.push(String::from(prefix));
            params.push(String::from("5"));
            params.push(idx.to_string());
        }
        SgrColor::Rgb(r, g, b) => {
            params.push(String::from(prefix));
            params.push(String::from("2"));
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
}

fn split_sync_block_hook_line_prefix(raw: &[u8]) -> Option<(bool, Vec<u8>)> {
    let raw_len = last_complete_hook_line_raw_len(raw)?;
    if raw_len >= raw.len() {
        return None;
    }

    let line_raw = raw.get(..raw_len)?;
    let trailing_suffix = raw.get(raw_len..)?;
    let line = pending_line_from_raw(line_raw);
    if !complete_hook_line(line.visible.trim()) {
        return None;
    }

    let keep_tail = sync_hook_tail_has_meaningful_bytes(trailing_suffix);
    let mut trailing_raw = Vec::new();
    if keep_tail {
        trailing_raw.extend_from_slice(&sgr_restore_bytes_for_prefix(line_raw));
        trailing_raw.extend_from_slice(trailing_suffix);
    }

    Some((keep_tail, trailing_raw))
}

fn last_complete_hook_line_raw_len(raw: &[u8]) -> Option<usize> {
    let mut pending = PendingLine::new();
    let mut complete_hook_line_raw_len = None;

    for &byte in raw {
        pending.raw.push(byte);
        pending.record_visible(byte);
        if complete_hook_line(pending.visible.trim()) {
            complete_hook_line_raw_len = Some(pending.raw.len());
        }
    }

    complete_hook_line_raw_len
}

fn sync_hook_tail_has_meaningful_bytes(raw: &[u8]) -> bool {
    let mut idx = 0usize;
    while let Some(&byte) = raw.get(idx) {
        match byte {
            b'\r' | b'\n' => idx += 1,
            0x1B if raw.get(idx + 1) == Some(&b'[') => {
                let params_start = idx + 2;
                let Some(params_tail) = raw.get(params_start..) else {
                    return true;
                };
                let Some(final_rel_idx) =
                    params_tail.iter().position(|param_byte| matches!(param_byte, 0x40..=0x7E))
                else {
                    return true;
                };
                let final_idx = params_start + final_rel_idx;
                if raw.get(final_idx) == Some(&b'm') {
                    idx = final_idx + 1;
                } else {
                    return true;
                }
            }
            _ => return true,
        }
    }

    false
}

fn indexed_sgr_color(value: u16) -> Option<SgrColor> {
    u8::try_from(value).ok().map(SgrColor::Indexed)
}

fn rgb_sgr_color(r: u16, g: u16, b: u16) -> Option<SgrColor> {
    Some(SgrColor::Rgb(u8::try_from(r).ok()?, u8::try_from(g).ok()?, u8::try_from(b).ok()?))
}

fn sgr_restore_bytes_for_prefix(raw: &[u8]) -> Vec<u8> {
    let mut state = SgrStyleState::default();
    let mut idx = 0usize;

    while idx < raw.len() {
        if raw.get(idx..idx + 2) != Some(b"\x1b[") {
            idx += 1;
            continue;
        }

        let params_start = idx + 2;
        let Some(params_tail) = raw.get(params_start..) else {
            break;
        };
        let Some(final_rel_idx) = params_tail.iter().position(|byte| matches!(byte, 0x40..=0x7E))
        else {
            break;
        };
        let final_idx = params_start + final_rel_idx;
        if raw.get(final_idx) == Some(&b'm') {
            let Some(params_raw) = raw.get(params_start..final_idx) else {
                break;
            };
            apply_sgr_sequence(&mut state, params_raw);
        }
        idx = final_idx + 1;
    }

    state.into_restore_bytes()
}

fn apply_sgr_sequence(state: &mut SgrStyleState, params_raw: &[u8]) {
    let params: Vec<u16> = if params_raw.is_empty() {
        vec![0]
    } else {
        String::from_utf8_lossy(params_raw)
            .split([';', ':'])
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect()
    };

    let mut idx = 0usize;
    while let Some(param) = params.get(idx).copied() {
        match param {
            0 => *state = SgrStyleState::default(),
            1 => state.set_flag(SgrStyleState::FLAG_BOLD, true),
            2 => state.set_flag(SgrStyleState::FLAG_DIM, true),
            3 => state.set_flag(SgrStyleState::FLAG_ITALIC, true),
            4 => state.set_flag(SgrStyleState::FLAG_UNDERLINE, true),
            7 => state.set_flag(SgrStyleState::FLAG_INVERSE, true),
            8 => state.set_flag(SgrStyleState::FLAG_HIDDEN, true),
            9 => state.set_flag(SgrStyleState::FLAG_STRIKETHROUGH, true),
            22 => {
                state.set_flag(SgrStyleState::FLAG_BOLD, false);
                state.set_flag(SgrStyleState::FLAG_DIM, false);
            }
            23 => state.set_flag(SgrStyleState::FLAG_ITALIC, false),
            24 => state.set_flag(SgrStyleState::FLAG_UNDERLINE, false),
            27 => state.set_flag(SgrStyleState::FLAG_INVERSE, false),
            28 => state.set_flag(SgrStyleState::FLAG_HIDDEN, false),
            29 => state.set_flag(SgrStyleState::FLAG_STRIKETHROUGH, false),
            30..=37 => state.fg = indexed_sgr_color(param - 30),
            39 => state.fg = None,
            40..=47 => state.bg = indexed_sgr_color(param - 40),
            49 => state.bg = None,
            90..=97 => state.fg = indexed_sgr_color(param - 90 + 8),
            100..=107 => state.bg = indexed_sgr_color(param - 100 + 8),
            38 | 48 => {
                let is_bg = param == 48;
                let consumed = apply_extended_sgr_color(state, &params, idx + 1, is_bg);
                idx += consumed;
            }
            _ => {}
        }
        idx += 1;
    }
}

fn apply_extended_sgr_color(
    state: &mut SgrStyleState,
    params: &[u16],
    start_idx: usize,
    background: bool,
) -> usize {
    let Some(mode) = params.get(start_idx).copied() else {
        return 0;
    };

    let color = match mode {
        2 => {
            let Some(rgb_params) = params.get(start_idx + 1..start_idx + 4) else {
                return 0;
            };
            let [r, g, b] = rgb_params else {
                return 0;
            };
            rgb_sgr_color(*r, *g, *b)
        }
        5 => params.get(start_idx + 1).and_then(|idx| indexed_sgr_color(*idx)),
        _ => None,
    };

    if let Some(color) = color {
        if background {
            state.bg = Some(color);
        } else {
            state.fg = Some(color);
        }
    }

    match mode {
        2 => 4,
        5 => 2,
        _ => 0,
    }
}

fn pending_line_from_raw(raw: &[u8]) -> PendingLine {
    let mut pending = PendingLine::new();
    for &byte in raw {
        pending.raw.push(byte);
        pending.record_visible(byte);
    }
    pending
}

fn is_hook_visible_line(s: &str) -> bool {
    is_hook_block_start_line(s) || is_hook_block_end_line(s)
}

#[cfg(test)]
mod tests {
    use super::{CodexHookLogFilter, CodexHookLogOutput};

    fn filter_all(input: &[u8]) -> Vec<u8> {
        let mut filter = CodexHookLogFilter::new();
        let mut output = match filter.filter(input) {
            CodexHookLogOutput::Unchanged(bytes) => bytes.to_vec(),
            CodexHookLogOutput::Filtered(bytes) => bytes,
        };
        if let Some(flushed) = filter.flush() {
            output.extend_from_slice(&flushed);
        }
        output
    }

    #[test]
    fn hides_permission_request_legacy_hook_block() {
        let input = b"before\nRunning PermissionRequest hook: checking policy\nPermissionRequest hook (completed)\n\nafter\n";

        assert_eq!(filter_all(input), b"before\nafter\n");
    }

    #[test]
    fn hides_permission_request_labeled_hook_block() {
        let input = b"before\n\x1b[32mhook: PermissionRequest\x1b[0m\n\x1b[32mhook: PermissionRequest Completed\x1b[0m\n\nafter\n";

        assert_eq!(filter_all(input), b"before\nafter\n");
    }

    #[test]
    fn trims_permission_request_hook_rows_inside_sync_update() {
        let input = b"\x1b[?2026hline 1\n\x1b[32mhook: PermissionRequest\x1b[0m\n\x1b[32mhook: PermissionRequest Completed\x1b[0m\nline 2\n\x1b[?2026l";

        assert_eq!(filter_all(input), b"\x1b[?2026hline 1\nline 2\n\x1b[?2026l");
    }
}
