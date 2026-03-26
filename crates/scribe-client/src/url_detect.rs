//! URL scanning and per-pane URL span cache.
//!
//! Scans the visible terminal grid for URLs and maintains a dirty-flag cache
//! so URL hit-testing can be performed without re-scanning every frame.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};

use crate::selection::read_cell_char;

/// A URL found on the terminal grid.
pub struct UrlSpan {
    /// Absolute grid row (0 = viewport top, negative = scrollback).
    pub row: i32,
    /// Column of the first character of the URL (inclusive).
    pub col_start: usize,
    /// Column of the last character of the URL (inclusive).
    pub col_end: usize,
    /// The URL text.
    pub url: String,
}

/// Per-pane cache of detected URL spans.
pub struct PaneUrlCache {
    spans: Vec<UrlSpan>,
    dirty: bool,
}

impl Default for PaneUrlCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneUrlCache {
    pub fn new() -> Self {
        Self { spans: Vec::new(), dirty: true }
    }

    /// Mark the cache as needing a re-scan.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Re-scan visible rows if the cache is dirty, then clear the dirty flag.
    pub fn refresh(&mut self, term: &Term<VoidListener>) {
        if !self.dirty {
            return;
        }
        self.spans = scan_visible_urls(term);
        self.dirty = false;
    }

    /// Return the `UrlSpan` whose column range contains `col` on `row`, if any.
    pub fn url_at(&self, row: i32, col: usize) -> Option<&UrlSpan> {
        self.spans.iter().find(|s| s.row == row && col >= s.col_start && col <= s.col_end)
    }

    /// All detected URL spans for the current viewport.
    pub fn visible_spans(&self) -> &[UrlSpan] {
        &self.spans
    }
}

/// URL schemes recognised by the scanner.
const PREFIXES: &[&str] = &["https://", "http://", "ftp://", "file://"];

/// Characters that terminate a URL when encountered (in addition to whitespace).
const URL_TERMINATORS: &[char] = &['<', '>', '"', '\'', '|'];

/// Punctuation that is stripped from the end of a URL when the corresponding
/// opening bracket is absent from the URL body.
const TRAILING_PUNCT: &[char] = &['.', ',', ')', ']', ';', ':', '!', '?'];

/// Bracket pairs checked when stripping trailing punctuation.
const BRACKET_PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']')];

/// Return `true` if `ch` ends URL collection.
fn is_url_terminator(ch: char) -> bool {
    ch.is_whitespace() || URL_TERMINATORS.contains(&ch)
}

/// Scan all visible rows of `term` for URLs and return their spans.
///
/// Row indices in the returned spans are **absolute grid lines**: screen row
/// minus `display_offset`, matching `alacritty_terminal`'s `Line` convention.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "screen_lines and display_offset are bounded by scrollback_lines (≤ 100_000), fit in i32"
)]
fn scan_visible_urls(term: &Term<VoidListener>) -> Vec<UrlSpan> {
    let rows = term.grid().screen_lines();
    let cols = term.grid().columns();
    let display_offset = term.grid().display_offset();

    let mut spans = Vec::new();

    let mut screen_row: usize = 0;
    while screen_row < rows {
        let row_abs = screen_row as i32 - display_offset as i32;
        let line = Line(row_abs);

        // Build the row text; each cell contributes exactly one char.
        let mut row_text = String::with_capacity(cols);
        let mut col_idx = 0usize;
        while col_idx < cols {
            let c = read_cell_char(term, line, Column(col_idx));
            row_text.push(c);
            col_idx = col_idx.saturating_add(1);
        }

        scan_row_urls(&row_text, cols, row_abs, &mut spans);

        screen_row = screen_row.saturating_add(1);
    }

    spans
}

/// Scan a single row's text for URLs and push found spans into `out`.
///
/// `row_text` contains exactly one `char` per terminal column (as built by
/// `scan_visible_urls`).  We therefore work exclusively with char indices so
/// that multi-byte characters (emoji, CJK, box-drawing) never cause a slice
/// at a non-char-boundary.
fn scan_row_urls(row_text: &str, cols: usize, row_abs: i32, out: &mut Vec<UrlSpan>) {
    // Collect into a Vec<char> so we can use char-level indexing without any
    // byte-offset arithmetic.
    let chars: Vec<char> = row_text.chars().collect();
    let char_count = chars.len();
    let mut char_pos = 0usize;

    while char_pos < char_count {
        let Some(prefix_len_chars) = match_prefix_chars(&chars, char_pos) else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };

        let url_col_start = char_pos;
        let url_col_end_raw = collect_url_end_chars(&chars, char_pos + prefix_len_chars);
        #[allow(
            clippy::indexing_slicing,
            reason = "url_col_end_raw is returned by collect_url_end_chars which is bounded by chars.len()"
        )]
        let raw: String = chars[url_col_start..url_col_end_raw].iter().collect();
        let url = strip_trailing_punct(raw);
        let url_char_len = url.chars().count();
        let url_col_end = url_col_start + url_char_len;

        if url_char_len > prefix_len_chars && url_col_end <= cols {
            let col_end = url_col_end.saturating_sub(1);
            out.push(UrlSpan { row: row_abs, col_start: url_col_start, col_end, url });
        }

        char_pos = url_col_end.max(char_pos.saturating_add(1));
    }
}

/// Match a URL prefix starting at `chars[pos]`, returning the prefix length in
/// chars if found.
fn match_prefix_chars(chars: &[char], pos: usize) -> Option<usize> {
    for prefix in PREFIXES {
        let prefix_chars: Vec<char> = prefix.chars().collect();
        let prefix_len = prefix_chars.len();
        #[allow(
            clippy::indexing_slicing,
            reason = "pos + prefix_len <= chars.len() is verified in the enclosing condition"
        )]
        if pos + prefix_len <= chars.len() && chars[pos..pos + prefix_len] == prefix_chars[..] {
            return Some(prefix_len);
        }
    }
    None
}

/// Walk forward from `start` (char index) collecting URL characters; return
/// the char index one past the last URL character.
fn collect_url_end_chars(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while end < chars.len() {
        #[allow(clippy::indexing_slicing, reason = "end < chars.len() is the loop condition")]
        if is_url_terminator(chars[end]) {
            break;
        }
        end = end.saturating_add(1);
    }
    end
}

/// Strip trailing punctuation from a URL, respecting bracket pairs.
fn strip_trailing_punct(mut url: String) -> String {
    while let Some(last) = url.chars().next_back() {
        if !TRAILING_PUNCT.contains(&last) {
            break;
        }

        let should_strip = BRACKET_PAIRS
            .iter()
            .find(|(_, close)| *close == last)
            .is_none_or(|(open, _)| !url.contains(*open));

        if should_strip {
            url.truncate(url.len() - last.len_utf8());
        } else {
            break;
        }
    }
    url
}

/// Open a URL in the system default browser.
///
/// Only `http://`, `https://`, `ftp://`, and `file://` URLs are accepted.
/// The child process is spawned and not awaited (fire-and-forget).
pub fn open_url(url: &str) {
    if !PREFIXES.iter().any(|p| url.starts_with(p)) {
        tracing::warn!("open_url: refusing to open non-http(s)/ftp/file URL");
        return;
    }

    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let cmd = "xdg-open";

    match std::process::Command::new(cmd).arg(url).spawn() {
        Ok(_child) => {}
        Err(e) => tracing::warn!("open_url: failed to spawn {cmd}: {e}"),
    }
}
