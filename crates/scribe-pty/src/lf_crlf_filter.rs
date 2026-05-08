/// Result of filtering: either the original bytes (no bare LF rewritten) or
/// a new buffer with bare LFs upgraded to CRLF.
pub enum LfCrlfOutput<'a> {
    /// No bare LF found — original bytes unchanged.
    Unchanged(&'a [u8]),
    /// One or more bare LFs were upgraded to CRLF — use this filtered buffer.
    Filtered(Vec<u8>),
}

impl LfCrlfOutput<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            LfCrlfOutput::Unchanged(b) => b,
            LfCrlfOutput::Filtered(v) => v,
        }
    }
}

/// State machine that upgrades bare `\n` (LF without a preceding `\r`) to
/// `\r\n` in PTY output, working around an off-by-one in
/// `alacritty_terminal` 0.26.0's `linefeed` handler that fails to clear
/// `input_needs_wrap` after a print-at-last-column with DECAWM.
///
/// xterm clears the deferred-wrap flag on LF (so `\n` and `\r\n` behave the
/// same after a last-column print). `alacritty_terminal::term::linefeed`
/// does not, so a wrap+LF pair advances the cursor by 2 visual rows instead
/// of 1, breaking cursor-up redraws like `release.sh`'s bash progress
/// panel. `\r` (`carriage_return`) does clear the flag, so prepending `\r`
/// before any bare LF restores xterm-equivalent behaviour without touching
/// already-CRLF streams.
///
/// Only the previous byte of the stream is needed to classify the next LF,
/// so the state across `filter()` calls is a single bool.
pub struct LfCrlfFilter {
    /// `true` iff the most recently emitted byte was `\r`.
    prev_was_cr: bool,
}

const LF: u8 = b'\n';
const CR: u8 = b'\r';

impl LfCrlfFilter {
    #[must_use]
    pub fn new() -> Self {
        Self { prev_was_cr: false }
    }

    /// Filter `input`, prepending `\r` before any bare LF.
    ///
    /// State is carried across calls so a chunk ending in `\r` followed by a
    /// chunk starting with `\n` is correctly recognised as already-CRLF.
    pub fn filter<'a>(&mut self, input: &'a [u8]) -> LfCrlfOutput<'a> {
        if !input.contains(&LF) {
            self.prev_was_cr = input.last().copied() == Some(CR);
            return LfCrlfOutput::Unchanged(input);
        }

        let mut out: Vec<u8> = Vec::with_capacity(input.len() + 4);
        let mut modified = false;

        for &byte in input {
            modified |= self.process_byte(byte, &mut out);
        }

        if modified { LfCrlfOutput::Filtered(out) } else { LfCrlfOutput::Unchanged(input) }
    }

    /// Process a single byte, emitting `\r\n` for a bare LF and the byte
    /// itself otherwise. Returns `true` when output diverged from a
    /// pass-through copy.
    fn process_byte(&mut self, byte: u8, out: &mut Vec<u8>) -> bool {
        if byte == LF && !self.prev_was_cr {
            out.push(CR);
            out.push(LF);
            self.prev_was_cr = false;
            return true;
        }
        out.push(byte);
        self.prev_was_cr = byte == CR;
        false
    }
}

impl Default for LfCrlfFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_when_no_lf() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"hello world");
        assert!(matches!(out, LfCrlfOutput::Unchanged(_)));
        assert_eq!(out.as_bytes(), b"hello world");
    }

    #[test]
    fn upgrades_bare_lf_to_crlf() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"abc\ndef");
        assert_eq!(out.as_bytes(), b"abc\r\ndef");
    }

    #[test]
    fn leaves_existing_crlf_unchanged() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"abc\r\ndef");
        assert!(matches!(out, LfCrlfOutput::Unchanged(_)));
        assert_eq!(out.as_bytes(), b"abc\r\ndef");
    }

    #[test]
    fn upgrades_only_bare_lfs_in_mixed_stream() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"a\nb\r\nc\nd");
        assert_eq!(out.as_bytes(), b"a\r\nb\r\nc\r\nd");
    }

    #[test]
    fn upgrades_consecutive_lfs() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"a\n\nb");
        assert_eq!(out.as_bytes(), b"a\r\n\r\nb");
    }

    #[test]
    fn handles_cr_split_across_chunks() {
        let mut f = LfCrlfFilter::new();
        let first = f.filter(b"abc\r");
        assert!(matches!(first, LfCrlfOutput::Unchanged(_)));
        assert_eq!(first.as_bytes(), b"abc\r");
        let second = f.filter(b"\ndef");
        assert_eq!(second.as_bytes(), b"\ndef");
    }

    #[test]
    fn handles_bare_lf_split_across_chunks() {
        let mut f = LfCrlfFilter::new();
        let first = f.filter(b"abc");
        assert_eq!(first.as_bytes(), b"abc");
        let second = f.filter(b"\ndef");
        assert_eq!(second.as_bytes(), b"\r\ndef");
    }

    #[test]
    fn lf_at_start_of_stream_gets_crlf() {
        let mut f = LfCrlfFilter::new();
        let out = f.filter(b"\nfoo");
        assert_eq!(out.as_bytes(), b"\r\nfoo");
    }
}
