/// Result of filtering: original bytes, or a buffer with the truncation EL neutralized.
pub enum ClaudePickerOutput<'a> {
    Unchanged(&'a [u8]),
    Filtered(Vec<u8>),
}

impl ClaudePickerOutput<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            ClaudePickerOutput::Unchanged(b) => b,
            ClaudePickerOutput::Filtered(v) => v,
        }
    }
}

/// State machine that detects Claude Code's picker-truncation 3rd-redraw
/// signature and neutralises the trailing `\x1b[K` (erase-to-end-of-line) so
/// the previously rendered prefix text on the input row survives the redraw.
///
/// Background: Ink's `AskUserQuestion` "Other" custom-text input emits a 3-stage
/// redraw when typed text overflows the picker's 2-row input field:
///   1. Re-emit the visible prefix text on the input row.
///   2. Overlay an `…` truncation indicator at the last column.
///   3. Re-position to the input row, print `❯`, skip 3 cols, print `…`, then
///      `\x1b[K` to erase the rest of the line.
///
/// Step 3's `\x1b[K` lands on the wrong row in `alacritty_terminal` 0.26.0
/// (an off-by-one vs xterm in cursor row tracking after a print-at-last-column
/// with DECAWM followed by `\r\n`), erasing the user's typed text.
///
/// We detect the 18-byte signature `❯\x1b[3C\x1b[39m…\x1b[K` and rewrite the
/// trailing 3-byte EL with NULs, which `alacritty_terminal`'s parser drops.
/// The earlier redraws' visible prefix + `…` then remain on screen.
pub struct ClaudePickerTruncationFilter {
    /// Number of leading bytes of the signature matched so far (`0..SIGNATURE.len()`).
    state: u8,
}

/// Distinctive 18-byte signature emitted by Ink at the picker-truncation moment.
///
///   `❯` (UTF-8 e2 9d af) + `\x1b[3C` + `\x1b[39m` + `…` (UTF-8 e2 80 a6) + `\x1b[K`
const SIGNATURE: &[u8] = b"\xe2\x9d\xaf\x1b[3C\x1b[39m\xe2\x80\xa6\x1b[K";

/// First byte of [`SIGNATURE`]. Hoisted so the fast-path check and reset
/// branch in [`ClaudePickerTruncationFilter::process_byte`] avoid panicking
/// indexing into a slice constant.
const SIGNATURE_FIRST: u8 = 0xe2;

impl ClaudePickerTruncationFilter {
    #[must_use]
    pub fn new() -> Self {
        Self { state: 0 }
    }

    /// Filter `input`, neutralising any complete truncation signature by
    /// replacing all 18 bytes with NULs (which `alacritty_terminal` discards).
    ///
    /// Partial matches at the end of `input` are held in state until the next
    /// call (or `flush()`).
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> ClaudePickerOutput<'a> {
        // Fast path: nothing pending and no possible signature start.
        if self.state == 0 && !input.contains(&SIGNATURE_FIRST) {
            return ClaudePickerOutput::Unchanged(input);
        }

        let mut out: Vec<u8> = Vec::with_capacity(input.len());
        let mut modified = false;

        for &byte in input {
            modified |= self.process_byte(byte, &mut out);
        }

        if modified || self.state > 0 {
            ClaudePickerOutput::Filtered(out)
        } else {
            ClaudePickerOutput::Unchanged(input)
        }
    }

    fn process_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        let expected = SIGNATURE.get(usize::from(self.state)).copied().unwrap_or(0);
        if byte == expected {
            self.state += 1;
            if usize::from(self.state) == SIGNATURE.len() {
                // Full signature matched. Replace all 18 bytes with NULs so
                // none of the 3rd-redraw overlay (`❯`, `…` at col 4, trailing
                // EL) reaches the grid. The earlier redraws' typed prefix and
                // trailing `…` truncation indicator on the input row survive.
                out.extend_from_slice(&[0u8; SIGNATURE.len()]);
                self.state = 0;
                return true;
            }
            return false;
        }

        // Mismatch: flush any pending partial match, then reconsider this byte.
        let diverged = self.state > 0;
        if diverged {
            if let Some(slice) = SIGNATURE.get(..usize::from(self.state)) {
                out.extend_from_slice(slice);
            }
            self.state = 0;
        }

        if byte == SIGNATURE_FIRST {
            self.state = 1;
        } else {
            out.push(byte);
        }
        diverged
    }

    /// Flush any bytes withheld in a partial match. Call at end-of-stream.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.state == 0 {
            return None;
        }
        let pending =
            SIGNATURE.get(..usize::from(self.state)).map(<[u8]>::to_vec).unwrap_or_default();
        self.state = 0;
        Some(pending)
    }
}

impl Default for ClaudePickerTruncationFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutralises_complete_signature() {
        let mut f = ClaudePickerTruncationFilter::new();
        let input = b"prefix\xe2\x9d\xaf\x1b[3C\x1b[39m\xe2\x80\xa6\x1b[Ksuffix";
        let out = f.filter(input);
        let bytes = out.as_bytes();
        // All 18 bytes of the signature are replaced with NULs.
        assert_eq!(&bytes[..6], b"prefix");
        assert_eq!(&bytes[6..24], &[0u8; 18]);
        assert_eq!(&bytes[24..], b"suffix");
    }

    #[test]
    fn passes_through_non_matching_bytes() {
        let mut f = ClaudePickerTruncationFilter::new();
        let input = b"hello world\x1b[K\x1b[2J";
        let out = f.filter(input);
        assert!(matches!(out, ClaudePickerOutput::Unchanged(_)));
        assert_eq!(out.as_bytes(), input);
    }

    #[test]
    fn handles_signature_split_across_chunks() {
        let mut f = ClaudePickerTruncationFilter::new();
        let first = f.filter(b"\xe2\x9d\xaf\x1b[3C\x1b[39m");
        // Partial match: nothing emitted yet.
        assert!(first.as_bytes().is_empty());
        let second = f.filter(b"\xe2\x80\xa6\x1b[K trailing");
        let bytes = second.as_bytes();
        // Signature reassembled across chunks: full 18 bytes replaced with NULs.
        assert_eq!(&bytes[..18], &[0u8; 18]);
        assert_eq!(&bytes[18..], b" trailing");
    }

    #[test]
    fn diverged_partial_match_is_flushed_unchanged() {
        let mut f = ClaudePickerTruncationFilter::new();
        // Start signature, then diverge — partial bytes must be emitted intact.
        let out = f.filter(b"\xe2\x9d\xaf\x1b[3CXYZ");
        assert_eq!(out.as_bytes(), b"\xe2\x9d\xaf\x1b[3CXYZ");
    }

    #[test]
    fn flush_returns_partial_state() {
        let mut f = ClaudePickerTruncationFilter::new();
        let first = f.filter(b"\xe2\x9d\xaf\x1b[3C");
        assert!(first.as_bytes().is_empty());
        assert_eq!(f.flush(), Some(b"\xe2\x9d\xaf\x1b[3C".to_vec()));
        assert_eq!(f.flush(), None);
    }
}
