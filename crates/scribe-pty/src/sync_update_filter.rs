/// Result of filtering: either the original bytes (no sync escapes removed) or
/// a new buffer with synchronized-update markers stripped.
pub enum SyncUpdateOutput<'a> {
    /// No synchronized-update markers found — original bytes unchanged.
    Unchanged(&'a [u8]),
    /// One or more synchronized-update markers were removed.
    Filtered(Vec<u8>),
}

impl SyncUpdateOutput<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            SyncUpdateOutput::Unchanged(bytes) => bytes,
            SyncUpdateOutput::Filtered(bytes) => bytes,
        }
    }
}

/// State machine that strips VTE synchronized-update markers from a byte stream.
///
/// Handles sequences split across multiple `filter()` calls. Only the exact
/// `CSI ? 2026 h` and `CSI ? 2026 l` sequences are removed.
pub struct SyncUpdateFilter {
    pending: Vec<u8>,
}

/// Start/end synchronized update escapes.
const BSU_CSI: [u8; 8] = *b"\x1b[?2026h";
const ESU_CSI: [u8; 8] = *b"\x1b[?2026l";

impl SyncUpdateFilter {
    #[must_use]
    pub fn new() -> Self {
        Self { pending: Vec::with_capacity(BSU_CSI.len()) }
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Filter `input`, removing any complete synchronized-update escapes.
    ///
    /// Partial matches at the end of `input` are retained until the next call
    /// (or `flush()`).
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> SyncUpdateOutput<'a> {
        if self.pending.is_empty() && !input.contains(&0x1B) {
            return SyncUpdateOutput::Unchanged(input);
        }

        let mut out = Vec::with_capacity(input.len());
        let mut modified = false;

        for &byte in input {
            modified |= self.process_byte(byte, &mut out);
        }

        if modified || self.has_pending() {
            SyncUpdateOutput::Filtered(out)
        } else {
            SyncUpdateOutput::Unchanged(input)
        }
    }

    fn process_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        self.pending.push(byte);
        if is_sync_prefix(&self.pending) {
            if is_complete_sync_escape(&self.pending) {
                self.pending.clear();
                return true;
            }
            return false;
        }

        let diverged = !self.pending.is_empty();
        while !self.pending.is_empty() && !is_sync_prefix(&self.pending) {
            out.push(self.pending.remove(0));
        }
        diverged
    }

    /// Flush any bytes held in a partial match state.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }
}

impl Default for SyncUpdateFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Streaming splitter that strips synchronized-update markers and emits one
/// visible frame per commit.
///
/// Bytes inside an open `CSI ? 2026 h` / `CSI ? 2026 l` block are buffered
/// until the closing `l` arrives, which lets non-sync-aware UIs replay the
/// same committed animation frames instead of collapsing the whole burst.
pub struct SyncUpdateFrameFilter {
    pending: Vec<u8>,
    current: Vec<u8>,
    inside_sync: bool,
}

impl SyncUpdateFrameFilter {
    #[must_use]
    pub fn new() -> Self {
        Self { pending: Vec::with_capacity(BSU_CSI.len()), current: Vec::new(), inside_sync: false }
    }

    /// Strip sync markers from `input`, returning one visible frame per
    /// completed synchronized-update commit. Any non-sync bytes outside an
    /// open block are returned immediately as a tail frame.
    #[must_use]
    pub fn filter_frames(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();

        for &byte in input {
            self.pending.push(byte);
            if !is_sync_prefix(&self.pending) {
                self.drain_pending_non_sync();
                continue;
            }

            if self.pending == BSU_CSI {
                self.pending.clear();
                self.inside_sync = true;
                continue;
            }

            if self.pending == ESU_CSI {
                self.pending.clear();
                self.inside_sync = false;
                self.push_current_frame(&mut frames);
            }
        }

        if !self.inside_sync && !self.current.is_empty() {
            frames.push(std::mem::take(&mut self.current));
        }

        frames
    }

    /// Flush any pending partial escape bytes plus buffered visible content.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.drain_pending_non_sync();
        if !self.pending.is_empty() {
            self.current.append(&mut self.pending);
        }

        self.inside_sync = false;
        (!self.current.is_empty()).then(|| std::mem::take(&mut self.current))
    }

    fn drain_pending_non_sync(&mut self) {
        while !self.pending.is_empty() && !is_sync_prefix(&self.pending) {
            self.current.push(self.pending.remove(0));
        }
    }

    fn push_current_frame(&mut self, frames: &mut Vec<Vec<u8>>) {
        if let Some(frame) = (!self.current.is_empty()).then(|| std::mem::take(&mut self.current)) {
            frames.push(frame);
        }
    }
}

impl Default for SyncUpdateFrameFilter {
    fn default() -> Self {
        Self::new()
    }
}

/// Streaming splitter that preserves raw synchronized-update markers and emits
/// one raw frame per commit.
///
/// Unlike `SyncUpdateFrameFilter`, this keeps the original `CSI ? 2026 h/l`
/// bytes in the emitted frames so a sync-aware terminal parser can still apply
/// its normal buffering semantics after frame pacing has been decided.
pub struct SyncUpdateFrameSplitter {
    pending: Vec<u8>,
    current: Vec<u8>,
    inside_sync: bool,
}

impl SyncUpdateFrameSplitter {
    #[must_use]
    pub fn new() -> Self {
        Self { pending: Vec::with_capacity(BSU_CSI.len()), current: Vec::new(), inside_sync: false }
    }

    /// Preserve sync markers in `input`, returning one raw frame per
    /// completed synchronized-update commit. Bytes outside a sync block are
    /// returned immediately as a tail frame.
    #[must_use]
    pub fn split_frames(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();

        for &byte in input {
            self.pending.push(byte);
            if !is_sync_prefix(&self.pending) {
                self.drain_pending_non_sync();
                continue;
            }

            if self.pending == BSU_CSI {
                self.current.extend_from_slice(&BSU_CSI);
                self.pending.clear();
                self.inside_sync = true;
                continue;
            }

            if self.pending == ESU_CSI {
                self.current.extend_from_slice(&ESU_CSI);
                self.pending.clear();
                self.inside_sync = false;
                self.push_current_frame(&mut frames);
            }
        }

        if !self.inside_sync && !self.current.is_empty() {
            frames.push(std::mem::take(&mut self.current));
        }

        frames
    }

    /// Flush any pending partial escape bytes plus buffered raw content.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.drain_pending_non_sync();
        if !self.pending.is_empty() {
            self.current.append(&mut self.pending);
        }

        self.inside_sync = false;
        (!self.current.is_empty()).then(|| std::mem::take(&mut self.current))
    }

    fn drain_pending_non_sync(&mut self) {
        while !self.pending.is_empty() && !is_sync_prefix(&self.pending) {
            self.current.push(self.pending.remove(0));
        }
    }

    fn push_current_frame(&mut self, frames: &mut Vec<Vec<u8>>) {
        if let Some(frame) = (!self.current.is_empty()).then(|| std::mem::take(&mut self.current)) {
            frames.push(frame);
        }
    }
}

impl Default for SyncUpdateFrameSplitter {
    fn default() -> Self {
        Self::new()
    }
}

fn is_sync_prefix(bytes: &[u8]) -> bool {
    BSU_CSI.starts_with(bytes) || ESU_CSI.starts_with(bytes)
}

fn is_complete_sync_escape(bytes: &[u8]) -> bool {
    bytes == BSU_CSI || bytes == ESU_CSI
}
