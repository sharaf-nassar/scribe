//! Regex search over the terminal grid, cribbed from Zed's `terminal.rs`.
//!
//! Wraps the Alacritty fork's [`RegexSearch`] / [`RegexIter`] to collect every
//! match across scrollback and the active viewport, then cycles a highlighted
//! "current" match forward and backward with wraparound — the find-overlay
//! behaviour of the winit client, rebuilt against `alacritty_terminal_gpui`.

use alacritty_terminal_gpui::Term;
use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::grid::Dimensions as _;
use alacritty_terminal_gpui::index::{Boundary, Column, Direction, Point};
use alacritty_terminal_gpui::term::search::{Match, RegexIter, RegexSearch};

use crate::selection::SelectionPoint;

/// A single regex match on the grid, in absolute grid coordinates.
///
/// `start`/`end` are inclusive cell endpoints in reading order, matching the
/// [`crate::selection::SelectionRange`] coordinate convention (row 0 = viewport
/// top, negative rows = scrollback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
}

impl SearchMatch {
    fn from_range(range: &Match) -> Self {
        let start = range.start();
        let end = range.end();
        Self {
            start: SelectionPoint { row: start.line.0, col: start.column.0 },
            end: SelectionPoint { row: end.line.0, col: end.column.0 },
        }
    }
}

/// Compiled find-in-terminal state: the ordered match set plus the index of the
/// currently highlighted match.
#[derive(Debug, Clone)]
pub struct TerminalSearch {
    query: String,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
}

impl TerminalSearch {
    /// Compile `query` and collect every match across the whole terminal grid.
    ///
    /// Returns `None` when the regex fails to compile. An empty query, or a
    /// valid regex with no matches, yields an empty (but valid) search.
    pub fn new(term: &Term<VoidListener>, query: &str) -> Option<Self> {
        if query.is_empty() {
            return Some(Self { query: String::new(), matches: Vec::new(), current: None });
        }

        let mut regex = RegexSearch::new(query).ok()?;
        let matches = collect_matches(term, &mut regex);
        let current = if matches.is_empty() { None } else { Some(0) };
        Some(Self { query: query.to_owned(), matches, current })
    }

    /// The query this search was compiled from.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// All matches in reading order (top-to-bottom, left-to-right).
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// Total number of matches.
    pub fn len(&self) -> usize {
        self.matches.len()
    }

    /// `true` when the search found no matches.
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Index of the currently highlighted match, if any.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The currently highlighted match, if any.
    pub fn current(&self) -> Option<SearchMatch> {
        self.current.and_then(|index| self.matches.get(index).copied())
    }

    /// Advance the highlight to the next match downward, wrapping from the last
    /// match back to the first. Returns the newly selected match.
    pub fn select_next(&mut self) -> Option<SearchMatch> {
        self.cycle(Direction::Right)
    }

    /// Advance the highlight to the previous match upward, wrapping from the
    /// first match back to the last. Returns the newly selected match.
    pub fn select_prev(&mut self) -> Option<SearchMatch> {
        self.cycle(Direction::Left)
    }

    fn cycle(&mut self, direction: Direction) -> Option<SearchMatch> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }

        let len = self.matches.len();
        let next = self.current.map_or(0, |index| match direction {
            Direction::Right => (index + 1) % len,
            Direction::Left => (index + len - 1) % len,
        });
        self.current = Some(next);
        self.matches.get(next).copied()
    }
}

/// Iterate every regex match over the full grid, from the topmost scrollback
/// line to the bottom of the viewport, in reading order.
fn collect_matches(term: &Term<VoidListener>, regex: &mut RegexSearch) -> Vec<SearchMatch> {
    let grid = term.grid();
    let last_column = Column(grid.columns().saturating_sub(1));
    let start = Point::new(grid.topmost_line(), Column(0));
    let end = Point::new(grid.bottommost_line(), last_column);

    // Clamp defensively so a degenerate grid never yields an inverted range.
    if start > end {
        return Vec::new();
    }
    let start = start.grid_clamp(term, Boundary::Grid);
    let end = end.grid_clamp(term, Boundary::Grid);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .map(|range| SearchMatch::from_range(&range))
        .collect()
}

#[cfg(test)]
mod tests {
    use alacritty_terminal_gpui::event::VoidListener;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::{Config, Term};
    use vte::ansi::Processor;

    use super::TerminalSearch;

    #[derive(Clone, Copy)]
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

    fn term_with_output(cols: usize, rows: usize, output: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TestDims { cols, rows }, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, output);
        term
    }

    // @lat: [[test#GPUI Terminal Search#Cycles matches with wraparound]]
    #[gpui::test]
    fn cycles_matches_with_wraparound() {
        let term = term_with_output(40, 4, b"foo bar foo baz foo\r\nqux foo end");
        let mut search = TerminalSearch::new(&term, "foo").expect("valid regex");
        assert_eq!(search.len(), 4);
        assert_eq!(search.current_index(), Some(0));

        // Matches are returned top-to-bottom, left-to-right.
        let starts: Vec<_> = search.matches().iter().map(|m| (m.start.row, m.start.col)).collect();
        assert_eq!(starts, vec![(0, 0), (0, 8), (0, 16), (1, 4)]);

        // Forward cycling advances then wraps to the first match.
        assert_eq!(search.select_next().map(|m| m.start.col), Some(8));
        assert_eq!(search.select_next().map(|m| m.start.col), Some(16));
        assert_eq!(search.select_next().map(|m| (m.start.row, m.start.col)), Some((1, 4)));
        assert_eq!(search.current_index(), Some(3));
        assert_eq!(search.select_next().map(|m| (m.start.row, m.start.col)), Some((0, 0)));

        // Backward cycling wraps from the first match to the last.
        assert_eq!(search.select_prev().map(|m| (m.start.row, m.start.col)), Some((1, 4)));
        assert_eq!(search.select_prev().map(|m| m.start.col), Some(16));
    }

    // @lat: [[test#GPUI Terminal Search#Match endpoints cover the whole hit]]
    #[gpui::test]
    fn match_endpoints_cover_whole_hit() {
        let term = term_with_output(40, 2, b"see error_42 here");
        let search = TerminalSearch::new(&term, "error_[0-9]+").expect("valid regex");
        let hit = search.current().expect("one match");
        assert_eq!((hit.start.row, hit.start.col), (0, 4));
        assert_eq!((hit.end.row, hit.end.col), (0, 11));
    }

    // @lat: [[test#GPUI Terminal Search#Empty and unmatched queries stay valid]]
    #[gpui::test]
    fn empty_and_unmatched_queries_stay_valid() {
        let term = term_with_output(20, 2, b"nothing here");

        let empty = TerminalSearch::new(&term, "").expect("empty query is valid");
        assert!(empty.is_empty());
        assert_eq!(empty.current_index(), None);

        let mut unmatched = TerminalSearch::new(&term, "zzz").expect("valid regex");
        assert!(unmatched.is_empty());
        assert_eq!(unmatched.select_next(), None);
        assert_eq!(unmatched.current_index(), None);

        assert!(TerminalSearch::new(&term, "(unclosed").is_none());
    }
}
