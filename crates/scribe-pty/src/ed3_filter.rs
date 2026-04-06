/// Result of filtering: either the original bytes (no ED 3 found) or
/// a new buffer with ED 3 sequences rewritten to ED 2.
pub enum Ed3Output<'a> {
    /// No ED 3 found — original bytes unchanged.
    Unchanged(&'a [u8]),
    /// ED 3 sequences rewritten — use this filtered buffer instead.
    Filtered(Vec<u8>),
}

impl Ed3Output<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Ed3Output::Unchanged(b) => b,
            Ed3Output::Filtered(v) => v,
        }
    }
}

/// State machine that rewrites CSI ED 3 (`\x1b[3J`) to CSI ED 2 (`\x1b[2J`).
///
/// When an AI assistant sends ED 3 to wipe scrollback, preserve the visible
/// clear-screen effect but downgrade the scrollback wipe to a normal full-screen
/// clear. This keeps prior history while still letting the client repaint from
/// a clean viewport.
///
/// Handles sequences split across multiple `filter()` calls. Pending bytes
/// from a partial match are carried over until confirmed or flushed.
pub struct Ed3Filter {
    /// Number of leading bytes of the ED 3 sequence matched so far (0–3).
    state: u8,
}

/// The four-byte ED 3 sequence: ESC [ 3 J.
const SEQ: [u8; 4] = [0x1B, 0x5B, 0x33, 0x4A];

impl Ed3Filter {
    #[must_use]
    pub fn new() -> Self {
        Self { state: 0 }
    }

    /// Filter `input`, rewriting any complete `\x1b[3J` sequences to `\x1b[2J`.
    ///
    /// Partial matches at the end of `input` are held in state and not emitted
    /// until the next call (or `flush()`).
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> Ed3Output<'a> {
        // Fast path: no ESC byte and no pending state — pass through unchanged.
        if self.state == 0 && !input.contains(&0x1B) {
            return Ed3Output::Unchanged(input);
        }

        let mut out: Vec<u8> = Vec::with_capacity(input.len());
        let mut modified = false;

        for &byte in input {
            modified |= self.process_byte(byte, &mut out);
        }

        if modified || self.state > 0 {
            Ed3Output::Filtered(out)
        } else {
            Ed3Output::Unchanged(input)
        }
    }

    /// Process a single byte through the state machine.
    /// Returns `true` if the output diverged from a pass-through copy.
    fn process_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        #[allow(
            clippy::indexing_slicing,
            reason = "self.state is bounded to 0..=3 by the state machine; SEQ has length 4"
        )]
        let expected = SEQ[self.state as usize];

        if byte == expected {
            return self.advance_match(out);
        }

        // Mismatch — flush any pending partial-match bytes, then handle current byte.
        let diverged = if self.state > 0 {
            self.flush_pending(out);
            true
        } else {
            false
        };

        if byte == SEQ[0] {
            self.state = 1;
        } else {
            out.push(byte);
        }
        diverged
    }

    /// Advance the match. If the full 4-byte sequence is matched, emit ED 2.
    /// Returns `true` when a complete sequence was consumed (output diverged).
    fn advance_match(&mut self, out: &mut Vec<u8>) -> bool {
        self.state += 1;
        if self.state == 4 {
            self.state = 0;
            out.extend_from_slice(&[0x1B, 0x5B, 0x32, 0x4A]);
            return true;
        }
        false
    }

    /// Write pending partial-match bytes to `out` and reset state.
    fn flush_pending(&mut self, out: &mut Vec<u8>) {
        #[allow(
            clippy::indexing_slicing,
            reason = "self.state is bounded to 1..=3 here; SEQ has length 4"
        )]
        out.extend_from_slice(&SEQ[..self.state as usize]);
        self.state = 0;
    }

    /// Flush any bytes held in a partial match state.
    ///
    /// Call at end-of-stream to recover bytes that were withheld pending a
    /// potential sequence completion.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.state == 0 {
            return None;
        }
        #[allow(
            clippy::indexing_slicing,
            reason = "self.state is bounded to 1..=3 here; SEQ has length 4"
        )]
        let pending = SEQ[..self.state as usize].to_vec();
        self.state = 0;
        Some(pending)
    }
}

impl Default for Ed3Filter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Ed3Filter;

    #[test]
    fn rewrites_complete_ed3_sequence_to_ed2() {
        let mut filter = Ed3Filter::new();

        let output = filter.filter(b"before\x1b[3Jafter");

        assert_eq!(output.as_bytes(), b"before\x1b[2Jafter");
        assert_eq!(filter.flush(), None);
    }

    #[test]
    fn rewrites_split_ed3_sequence_across_chunks() {
        let mut filter = Ed3Filter::new();

        let first = filter.filter(b"before\x1b[");
        assert_eq!(first.as_bytes(), b"before");

        let second = filter.filter(b"3Jafter");

        assert_eq!(second.as_bytes(), b"\x1b[2Jafter");
        assert_eq!(filter.flush(), None);
    }

    #[test]
    fn leaves_other_escape_sequences_unchanged() {
        let mut filter = Ed3Filter::new();

        let output = filter.filter(b"before\x1b[2Jafter");

        assert_eq!(output.as_bytes(), b"before\x1b[2Jafter");
        assert_eq!(filter.flush(), None);
    }

    #[test]
    fn flush_returns_unmatched_partial_state() {
        let mut filter = Ed3Filter::new();

        let output = filter.filter(b"before\x1b[3");

        assert_eq!(output.as_bytes(), b"before");
        assert_eq!(filter.flush(), Some(b"\x1b[3".to_vec()));
        assert_eq!(filter.flush(), None);
    }
}
