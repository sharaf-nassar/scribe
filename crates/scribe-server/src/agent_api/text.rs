//! Bounded `Term` → text extractor for agent screen reads (spec 027).
//!
//! Two phases with an explicit lock boundary: [`copy_rows`] runs *under* the
//! terminal lock and only copies the requested rows; [`format_rows`] runs
//! *after* the caller releases the lock and performs all normalization
//! (soft-wrap joining, blank-tail trimming, image markers, byte capping).
//! Never hold the terminal lock across formatting.

use alacritty_terminal::Term;
use alacritty_terminal::grid::{Dimensions, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags as CellFlags};
use scribe_pty::event_listener::ScribeEventListener;

/// Kitty's private-use image placeholder cell — the only image protocol that
/// occupies grid cells. Mirrors
/// `scribe-client/src/terminal_image_scene.rs::KITTY_IMAGE_PLACEHOLDER`.
const KITTY_IMAGE_PLACEHOLDER: char = '\u{10eeee}';

/// Marker emitted in place of each run of image placeholder cells.
const IMAGE_OMITTED: &str = "[image omitted]";

/// One grid cell copied under the terminal lock. Styles, colors, and
/// hyperlink URIs are deliberately never copied: an OSC 8 link contributes
/// only its visible label text.
#[derive(Debug, Clone, Copy)]
pub struct CopiedCell {
    /// The cell's character content.
    pub c: char,
    /// True for wide-char spacer cells, which carry no content of their own.
    pub spacer: bool,
}

/// One grid row copied under the terminal lock.
#[derive(Debug, Clone)]
pub struct CopiedRow {
    /// The row's cells, leftmost first.
    pub cells: Vec<CopiedCell>,
    /// True when the row soft-wraps into the next row (`WRAPLINE`).
    pub soft_wrapped: bool,
}

/// Normalized text produced by [`format_rows`] outside the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedText {
    /// Logical lines joined with `\n`; no trailing newline.
    pub text: String,
    /// Number of logical lines present in `text`.
    pub lines: u32,
    /// True when the byte cap cut content off.
    pub truncated: bool,
}

/// Phase 1 — call while holding the terminal lock.
///
/// Copies the requested scrollback rows plus the viewport, oldest first, and
/// nothing else. The scrollback request clamps to `max_scrollback_lines`
/// (config ceiling) and to the history actually present. Alt-screen history
/// is a resize artifact rather than user content (see
/// `session_manager::snapshot_term`), so it is never served.
#[must_use]
pub fn copy_rows(
    term: &Term<ScribeEventListener>,
    requested_scrollback: u32,
    max_scrollback_lines: u32,
) -> Vec<CopiedRow> {
    let grid = term.grid();
    let columns = grid.columns();

    let history = if term.mode().contains(TermMode::ALT_SCREEN) { 0 } else { grid.history_size() };
    let capped = requested_scrollback.min(max_scrollback_lines);
    let scrollback = usize::try_from(capped).unwrap_or(usize::MAX).min(history);

    let mut rows = Vec::with_capacity(scrollback + grid.screen_lines());
    for offset in (1..=scrollback).rev() {
        let line = Line(-i32::try_from(offset).unwrap_or(i32::MAX));
        rows.push(copy_row(&grid[line], columns));
    }
    for index in 0..grid.screen_lines() {
        let line = Line(i32::try_from(index).unwrap_or(i32::MAX));
        rows.push(copy_row(&grid[line], columns));
    }
    rows
}

fn copy_row(row: &Row<Cell>, columns: usize) -> CopiedRow {
    let mut cells = Vec::with_capacity(columns);
    let mut soft_wrapped = false;
    for column in 0..columns {
        let cell = &row[Column(column)];
        // WRAPLINE lands on whichever cell ends the row — including a
        // leading wide-char spacer — so any cell may carry it.
        soft_wrapped |= cell.flags.contains(CellFlags::WRAPLINE);
        cells.push(CopiedCell {
            c: cell.c,
            spacer: cell
                .flags
                .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER),
        });
    }
    CopiedRow { cells, soft_wrapped }
}

/// Phase 2 — call after releasing the terminal lock.
///
/// Joins soft-wrapped rows into logical lines, preserves hard breaks, trims
/// each line's trailing blank cells and the blank tail of the viewport,
/// replaces image placeholder runs with [`IMAGE_OMITTED`], and enforces
/// `max_bytes` on the joined text, cutting at a char boundary and setting
/// `truncated` when content is dropped.
#[must_use]
pub fn format_rows(rows: &[CopiedRow], max_bytes: usize) -> ExtractedText {
    cap_bytes(&logical_lines(rows), max_bytes)
}

/// Join soft-wrapped rows, normalize each logical line, and drop the run of
/// blank lines at the bottom of the copied range.
fn logical_lines(rows: &[CopiedRow]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for row in rows {
        for cell in &row.cells {
            if !cell.spacer {
                current.push(cell.c);
            }
        }
        if !row.soft_wrapped {
            lines.push(normalize_line(std::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        // The final copied row soft-wraps past the copied range.
        lines.push(normalize_line(current));
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn normalize_line(mut line: String) -> String {
    line.truncate(line.trim_end_matches(' ').len());
    if line.contains(KITTY_IMAGE_PLACEHOLDER) { replace_image_runs(&line) } else { line }
}

/// Replace each maximal run of placeholder cells with one marker.
fn replace_image_runs(line: &str) -> String {
    let mut replaced = String::with_capacity(line.len());
    let mut in_run = false;
    for character in line.chars() {
        if character == KITTY_IMAGE_PLACEHOLDER {
            if !in_run {
                replaced.push_str(IMAGE_OMITTED);
                in_run = true;
            }
        } else {
            in_run = false;
            replaced.push(character);
        }
    }
    replaced
}

/// Join logical lines with `\n` under a byte budget.
fn cap_bytes(lines: &[String], max_bytes: usize) -> ExtractedText {
    let mut text = String::new();
    let mut count: u32 = 0;
    let mut truncated = false;
    for (index, line) in lines.iter().enumerate() {
        let separator = usize::from(index > 0);
        let fits = text.len() + separator + line.len() <= max_bytes;
        let available =
            if fits { line.len() } else { max_bytes.saturating_sub(text.len() + separator) };
        let cut = floor_char_boundary(line, available);
        if !fits && cut == 0 {
            truncated = true;
            break;
        }
        if separator == 1 {
            text.push('\n');
        }
        text.push_str(line.get(..cut).unwrap_or_default());
        count = count.saturating_add(1);
        if !fits {
            truncated = true;
            break;
        }
    }
    ExtractedText { text, lines: count, truncated }
}

/// Largest char-boundary index in `line` that is `<= index`.
fn floor_char_boundary(line: &str, index: usize) -> usize {
    let mut cut = index.min(line.len());
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions;
    use scribe_common::ids::SessionId;
    use scribe_pty::event_listener::ScribeEventListener;
    use tokio::sync::mpsc;
    use vte::ansi::Processor as AnsiProcessor;

    use super::{CopiedCell, CopiedRow, ExtractedText, copy_rows, format_rows};
    use crate::session_manager::build_term_config;

    struct TestDims {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestDims {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    /// Build a PTY-less `Term` and feed it raw bytes.
    fn term_with_bytes(bytes: &[u8], cols: usize, rows: usize) -> Term<ScribeEventListener> {
        let (tx, _rx) = mpsc::unbounded_channel();
        let listener = ScribeEventListener::new(SessionId::new(), tx);
        let mut term = Term::new(build_term_config(100), &TestDims { cols, rows }, listener);
        let mut processor: AnsiProcessor = AnsiProcessor::new();
        processor.advance(&mut term, bytes);
        term
    }

    /// Synthetic row of plain cells.
    fn row(text: &str, soft_wrapped: bool) -> CopiedRow {
        CopiedRow {
            cells: text.chars().map(|c| CopiedCell { c, spacer: false }).collect(),
            soft_wrapped,
        }
    }

    const NO_CAP: usize = 4096;

    #[test]
    fn soft_wrap_joins_into_one_logical_line() {
        let rows = [row("hello wor", true), row("ld   ", false)];
        let extracted = format_rows(&rows, NO_CAP);
        assert_eq!(
            extracted,
            ExtractedText { text: "hello world".into(), lines: 1, truncated: false }
        );
    }

    #[test]
    fn hard_breaks_are_preserved() {
        let rows = [row("one", false), row("two", false)];
        let extracted = format_rows(&rows, NO_CAP);
        assert_eq!(extracted.text, "one\ntwo");
        assert_eq!(extracted.lines, 2);
    }

    #[test]
    fn blank_tail_trimmed_but_interior_blanks_kept() {
        let rows =
            [row("a  ", false), row("", false), row("b", false), row("   ", false), row("", false)];
        let extracted = format_rows(&rows, NO_CAP);
        assert_eq!(extracted.text, "a\n\nb");
        assert_eq!(extracted.lines, 3);
    }

    #[test]
    fn wide_char_spacers_are_skipped() {
        let rows = [CopiedRow {
            cells: vec![
                CopiedCell { c: '好', spacer: false },
                CopiedCell { c: ' ', spacer: true },
                CopiedCell { c: '!', spacer: false },
            ],
            soft_wrapped: false,
        }];
        assert_eq!(format_rows(&rows, NO_CAP).text, "好!");
    }

    #[test]
    fn image_placeholder_runs_become_one_marker() {
        let placeholders: String = std::iter::repeat_n(super::KITTY_IMAGE_PLACEHOLDER, 3).collect();
        let rows = [row(&format!("{placeholders}a{placeholders}"), false)];
        assert_eq!(format_rows(&rows, NO_CAP).text, "[image omitted]a[image omitted]");
    }

    #[test]
    fn byte_cap_truncates_and_sets_flag() {
        let rows = [row("hello", false), row("world", false)];
        let extracted = format_rows(&rows, 7);
        assert_eq!(extracted, ExtractedText { text: "hello\nw".into(), lines: 2, truncated: true });
    }

    #[test]
    fn byte_cap_respects_char_boundaries() {
        let rows = [row("あい", false)];
        let extracted = format_rows(&rows, 4);
        assert_eq!(extracted, ExtractedText { text: "あ".into(), lines: 1, truncated: true });
    }

    #[test]
    fn exact_fit_is_not_truncated() {
        let rows = [row("hello", false), row("world", false)];
        let extracted = format_rows(&rows, 11);
        assert_eq!(
            extracted,
            ExtractedText { text: "hello\nworld".into(), lines: 2, truncated: false }
        );
    }

    #[test]
    fn osc8_label_is_kept_and_uri_dropped() {
        let term =
            term_with_bytes(b"\x1b]8;;https://example.com\x1b\\click here\x1b]8;;\x1b\\", 40, 4);
        let extracted = format_rows(&copy_rows(&term, 0, 1000), NO_CAP);
        assert_eq!(extracted.text, "click here");
        assert!(!extracted.text.contains("example.com"));
    }

    #[test]
    fn real_grid_wrap_and_wide_chars_join() {
        // 好 does not fit in column 3, so alacritty emits a leading
        // wide-char spacer carrying WRAPLINE and wraps the char itself.
        let term = term_with_bytes("abc好".as_bytes(), 4, 3);
        let extracted = format_rows(&copy_rows(&term, 0, 1000), NO_CAP);
        assert_eq!(extracted, ExtractedText { text: "abc好".into(), lines: 1, truncated: false });
    }

    #[test]
    fn scrollback_clamps_to_request_config_and_history() {
        // 2-row viewport (l5, l6) over 4 lines of history (l1..l4).
        let term = term_with_bytes(b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6", 4, 2);

        let all = format_rows(&copy_rows(&term, 100, 1000), NO_CAP);
        assert_eq!(all.text, "l1\nl2\nl3\nl4\nl5\nl6");

        let config_capped = format_rows(&copy_rows(&term, 100, 2), NO_CAP);
        assert_eq!(config_capped.text, "l3\nl4\nl5\nl6");

        let request_capped = format_rows(&copy_rows(&term, 1, 1000), NO_CAP);
        assert_eq!(request_capped.text, "l4\nl5\nl6");

        let viewport_only = format_rows(&copy_rows(&term, 0, 1000), NO_CAP);
        assert_eq!(viewport_only.text, "l5\nl6");
    }

    #[test]
    fn alt_screen_serves_no_scrollback() {
        let term = term_with_bytes(b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6\x1b[?1049h", 4, 2);
        assert_eq!(copy_rows(&term, 100, 1000).len(), 2);
    }
}
