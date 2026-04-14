//! URL scanning and per-pane URL span cache.
//!
//! Scans the visible terminal grid for URLs and maintains a dirty-flag cache
//! so URL hit-testing can be performed without re-scanning every frame.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};

use crate::selection::read_cell_char;

/// Whether a detected span is a URL or a file-system path.
#[derive(Clone, Copy)]
pub enum SpanKind {
    /// A recognised URL (`https://`, `http://`, `ftp://`, `file://`).
    Url,
    /// A file-system path (`/abs`, `~/`, `./`, `../`, or bare `word/path`).
    Path,
}

/// A URL or file path found on the terminal grid.
pub struct UrlSpan {
    /// Absolute grid row (0 = viewport top, negative = scrollback).
    pub row: i32,
    /// Column of the first character of the URL (inclusive).
    pub col_start: usize,
    /// Column of the last character of the URL (inclusive).
    pub col_end: usize,
    /// The URL or path text.
    pub url: String,
    /// Whether this span is a URL or a file path.
    pub kind: SpanKind,
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

fn grid_index_i32(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Scan all visible rows of `term` for URLs and return their spans.
///
/// Row indices in the returned spans are **absolute grid lines**: screen row
/// minus `display_offset`, matching `alacritty_terminal`'s `Line` convention.
fn scan_visible_urls(term: &Term<VoidListener>) -> Vec<UrlSpan> {
    let rows = term.grid().screen_lines();
    let cols = term.grid().columns();
    let display_offset = term.grid().display_offset();

    let mut spans = Vec::new();

    let mut screen_row: usize = 0;
    while screen_row < rows {
        let row_abs = grid_index_i32(screen_row).saturating_sub(grid_index_i32(display_offset));
        let line = Line(row_abs);

        // Build the row text; each cell contributes exactly one char.
        let mut row_text = String::with_capacity(cols);
        let mut col_idx = 0usize;
        while col_idx < cols {
            let c = read_cell_char(term, line, Column(col_idx));
            row_text.push(c);
            col_idx = col_idx.saturating_add(1);
        }

        let url_count_before = spans.len();
        scan_row_urls(&row_text, cols, row_abs, &mut spans);
        // Collect the URL spans just added into a temporary vec so the path
        // scanner can reference them without holding an immutable borrow on
        // `spans` while we also push into it.
        let row_url_spans: Vec<(usize, usize)> = spans
            .get(url_count_before..)
            .unwrap_or(&[])
            .iter()
            .map(|s| (s.col_start, s.col_end))
            .collect();
        let chars: Vec<char> = row_text.chars().collect();
        scan_row_paths(&chars, cols, row_abs, &row_url_spans, &mut spans);

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
        let raw: String = chars.get(url_col_start..url_col_end_raw).unwrap_or(&[]).iter().collect();
        let url = strip_trailing_punct(raw);
        let url_char_len = url.chars().count();
        let url_col_end = url_col_start + url_char_len;

        if url_char_len > prefix_len_chars && url_col_end <= cols {
            let col_end = url_col_end.saturating_sub(1);
            out.push(UrlSpan {
                row: row_abs,
                col_start: url_col_start,
                col_end,
                url,
                kind: SpanKind::Url,
            });
        }

        char_pos = url_col_end.max(char_pos.saturating_add(1));
    }
}

/// Match a URL prefix starting at `chars[pos]`, returning the prefix length in
/// chars if found.
fn match_prefix_chars(chars: &[char], pos: usize) -> Option<usize> {
    for prefix in PREFIXES {
        let prefix_len = prefix.chars().count();
        let matches = prefix
            .chars()
            .enumerate()
            .all(|(offset, prefix_char)| chars.get(pos + offset) == Some(&prefix_char));
        if matches {
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
        let Some(ch) = chars.get(end).copied() else {
            break;
        };
        if is_url_terminator(ch) {
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

/// Maximum lookahead for bare relative path detection (e.g. `src/main.rs`).
const BARE_PATH_LOOKAHEAD: usize = 30;

/// Scan a single row for file-system paths and push found spans into `out`.
///
/// `url_ranges` contains `(col_start, col_end)` pairs for URL spans already
/// detected on this row; any character position that falls inside one of them
/// is skipped to avoid overlaps.
fn scan_row_paths(
    chars: &[char],
    cols: usize,
    row_abs: i32,
    url_ranges: &[(usize, usize)],
    out: &mut Vec<UrlSpan>,
) {
    let char_count = chars.len();
    let mut char_pos = 0usize;

    while char_pos < char_count {
        // Skip positions that belong to a URL span.
        if url_ranges.iter().any(|(start, end)| char_pos >= *start && char_pos <= *end) {
            char_pos = char_pos.saturating_add(1);
            continue;
        }

        // Try to match a path prefix at this position.
        let Some((prefix_len, is_bare_relative)) = detect_path_prefix(chars, char_pos) else {
            char_pos = char_pos.saturating_add(1);
            continue;
        };

        let path_col_start = char_pos;
        let body_start = char_pos + prefix_len;
        let raw_end = collect_url_end_chars(chars, body_start);

        let raw: String = chars.get(path_col_start..raw_end).unwrap_or(&[]).iter().collect();
        let path = strip_trailing_punct(raw);
        let path_char_len = path.chars().count();
        let path_col_end_exclusive = path_col_start + path_char_len;

        // Bare relative paths must contain at least one '/' in the collected token.
        let valid = if is_bare_relative {
            path.contains('/') && path_char_len > prefix_len
        } else {
            path_char_len > prefix_len && path_col_end_exclusive <= cols
        };

        if valid && path_col_end_exclusive <= cols {
            let col_end = path_col_end_exclusive.saturating_sub(1);
            out.push(UrlSpan {
                row: row_abs,
                col_start: path_col_start,
                col_end,
                url: path,
                kind: SpanKind::Path,
            });
            char_pos = path_col_end_exclusive.max(char_pos.saturating_add(1));
        } else {
            char_pos = char_pos.saturating_add(1);
        }
    }
}

/// Attempt to match a file-system path prefix starting at `chars[pos]`.
///
/// Returns `(prefix_len, is_bare_relative)` on success, `None` otherwise.
fn detect_path_prefix(chars: &[char], pos: usize) -> Option<(usize, bool)> {
    // `~/` — home-relative.
    if chars.get(pos) == Some(&'~') && chars.get(pos + 1) == Some(&'/') {
        return Some((2, false));
    }

    // `../` — explicit relative.
    if chars.get(pos) == Some(&'.')
        && chars.get(pos + 1) == Some(&'.')
        && chars.get(pos + 2) == Some(&'/')
    {
        return Some((3, false));
    }

    // `./` — explicit relative.
    if chars.get(pos) == Some(&'.') && chars.get(pos + 1) == Some(&'/') {
        return Some((2, false));
    }

    // `/something` — absolute path: must be preceded by whitespace or BOL
    // and the character after `/` must not be another `/` or whitespace.
    if chars.get(pos) == Some(&'/') {
        let preceded_by_ws = pos == 0 || chars.get(pos - 1).is_some_and(|c| c.is_whitespace());
        let followed_ok = chars.get(pos + 1).is_some_and(|c| !c.is_whitespace() && *c != '/');
        if preceded_by_ws && followed_ok {
            return Some((1, false));
        }
        return None;
    }

    // Bare relative word containing `/` within BARE_PATH_LOOKAHEAD chars.
    // Require alphanumeric start and at least one `/` in the lookahead window.
    if chars.get(pos).is_some_and(char::is_ascii_alphanumeric) {
        let look_end = (pos + BARE_PATH_LOOKAHEAD).min(chars.len());
        let window = chars.get(pos..look_end).unwrap_or(&[]);
        // Ensure there is a `/` in the window and no space before it.
        let slash_pos = window.iter().position(|c| *c == '/');
        if let Some(rel_slash) = slash_pos {
            // No whitespace before the slash.
            let no_space =
                window.get(..rel_slash).unwrap_or(&[]).iter().all(|c| !c.is_whitespace());
            if no_space {
                return Some((0, true));
            }
        }
    }

    None
}

/// Open a file path with the system default application, optionally jumping
/// to a line number with VS Code when a `:N` suffix is present.
///
/// - Strips an optional `:N` line-number suffix from the end of `raw`.
/// - Expands `~/` to `$HOME/`.
/// - Resolves relative paths against `cwd` when provided.
/// - If a line number is present, tries `code --goto path:line` first;
///   falls back to `xdg-open` / `open` when VS Code is not found.
pub fn open_path(raw: &str, cwd: Option<&std::path::Path>) {
    use std::io::ErrorKind;

    // Parse optional :N line-number suffix.
    let (path_str, line_num) = parse_path_line_suffix(raw);

    // Expand ~/
    let expanded: String = path_str.strip_prefix("~/").map_or_else(
        || path_str.to_owned(),
        |rel| {
            std::env::var("HOME").ok().map_or_else(
                || path_str.to_owned(),
                |home| format!("{}/{rel}", home.trim_end_matches('/')),
            )
        },
    );

    // Resolve relative paths against cwd.
    let resolved = if expanded.starts_with('/') {
        expanded
    } else {
        match cwd {
            Some(base) => base.join(&expanded).to_string_lossy().into_owned(),
            None => expanded,
        }
    };

    #[cfg(target_os = "linux")]
    let open_cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let open_cmd = "open";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let open_cmd = "xdg-open";

    if let Some(line) = line_num {
        let goto_arg = format!("{resolved}:{line}");
        match std::process::Command::new("code").args(["--goto", &goto_arg]).spawn() {
            Ok(_child) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // VS Code not installed — fall through to xdg-open.
            }
            Err(e) => {
                tracing::warn!("open_path: failed to spawn code: {e}");
                return;
            }
        }
    }

    match std::process::Command::new(open_cmd).arg(&resolved).spawn() {
        Ok(_child) => {}
        Err(e) => tracing::warn!("open_path: failed to spawn {open_cmd}: {e}"),
    }
}

/// Split a raw path string into `(path, optional_line_number)`.
///
/// A trailing `:N` suffix is recognised only when `N` is a non-empty string
/// of ASCII digits.
fn parse_path_line_suffix(raw: &str) -> (&str, Option<u32>) {
    if let Some(colon) = raw.rfind(':') {
        let suffix = &raw[colon + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = suffix.parse::<u32>() {
                return (&raw[..colon], Some(n));
            }
        }
    }
    (raw, None)
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
