//! Terminal text selection: range tracking, text extraction, and interactive
//! selection state with copy-on-select.
//!
//! Ported from the winit client's `crates/scribe-client/src/selection.rs` onto
//! Zed's Alacritty fork (`alacritty_terminal_gpui`). Provides types for
//! tracking a selection range on the terminal grid, extracting selected text
//! (WRAPLINE-aware), and resolving cell/word/line granularity during a mouse
//! drag. Lowering a pointer position onto a cell is not done here: the paint
//! path owns the grid rect, so `terminal_element::cell_at` is the hit test and
//! `terminal::TerminalView::selection_point` applies the display offset.

use alacritty_terminal_gpui::Term;
use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::grid::Dimensions as _;
use alacritty_terminal_gpui::index::{Column, Line};
use alacritty_terminal_gpui::term::cell::{Cell, Flags};

/// Granularity of a terminal selection.
///
/// Mirrors the winit client's `mouse_state::SelectionMode`; the GPUI client
/// owns its own copy because the mouse-state module is ported in a later bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Select individual cells.
    Cell,
    /// Select whole words.
    Word,
    /// Select whole lines.
    Line,
}

/// A position on the terminal grid, in row/column coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: i32,
    pub col: usize,
}

impl SelectionPoint {
    /// Adjust the row by `delta` grid lines (positive = down, negative = up).
    pub fn shift_row(&mut self, delta: i32) {
        self.row += delta;
    }
}

impl PartialOrd for SelectionPoint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SelectionPoint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.row.cmp(&other.row).then(self.col.cmp(&other.col))
    }
}

/// A range of selected cells between two grid positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub mode: SelectionMode,
}

impl SelectionRange {
    /// Create a cell-granularity selection.
    pub fn cell(start: SelectionPoint, end: SelectionPoint) -> Self {
        Self { start, end, mode: SelectionMode::Cell }
    }

    /// Create a word-granularity selection.
    pub fn word(start: SelectionPoint, end: SelectionPoint) -> Self {
        Self { start, end, mode: SelectionMode::Word }
    }

    /// Create a line-granularity selection.
    pub fn line(start: SelectionPoint, end: SelectionPoint) -> Self {
        Self { start, end, mode: SelectionMode::Line }
    }

    /// Return `(start, end)` in reading order: top-to-bottom,
    /// left-to-right. The first element is always the earlier position.
    pub fn normalized(&self) -> (SelectionPoint, SelectionPoint) {
        if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Return `true` if the given cell lies within this selection range.
    pub fn contains_cell(&self, row: i32, col: usize) -> bool {
        let (lo, hi) = self.normalized();

        if row < lo.row || row > hi.row {
            return false;
        }

        if lo.row == hi.row {
            // Single-row selection.
            return col >= lo.col && col <= hi.col;
        }

        if row == lo.row {
            return col >= lo.col;
        }

        if row == hi.row {
            return col <= hi.col;
        }

        // Row is strictly between the first and last selected rows.
        true
    }

    /// Return `true` if the selection covers zero cells (start equals end).
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Adjust both endpoints by `delta` grid lines.
    pub fn shift_rows(&mut self, delta: i32) {
        self.start.shift_row(delta);
        self.end.shift_row(delta);
    }
}

/// Narrow a grid unit count to the `i32` grid-line space
/// `alacritty_terminal` indexes rows in, saturating rather than wrapping.
fn selection_grid_i32(units: usize) -> i32 {
    i32::try_from(units).unwrap_or(i32::MAX)
}

/// Extract the selected text from the terminal grid.
///
/// Selection rows are **absolute grid lines** (0 = viewport top, negative =
/// scrollback), matching the `Line` index used by `alacritty_terminal`.
/// Walks rows from the normalised start to the normalised end, collecting
/// cell characters.  Trailing spaces on each row are trimmed.  Rows that wrap
/// into the next row (WRAPLINE flag set on the last cell) are joined without
/// a newline; other row boundaries produce `'\n'`.
pub fn extract_text(term: &Term<VoidListener>, range: &SelectionRange) -> String {
    let (lo, hi) = range.normalized();

    let cols = term.grid().columns();
    let last_col = Column(cols.saturating_sub(1));
    let mut out = String::new();
    let mut prev_row: Option<i32> = None;

    let mut row = lo.row;
    while row <= hi.row {
        let line_obj = Line(row);

        let col_start = if row == lo.row { lo.col } else { 0 };
        let col_end = if row == hi.row { hi.col } else { cols.saturating_sub(1) };

        let mut line_buf = String::new();
        let mut col_idx = col_start;
        while col_idx <= col_end {
            let c = read_cell_char(term, line_obj, Column(col_idx));
            line_buf.push(c);
            col_idx = col_idx.saturating_add(1);
        }

        let trimmed = line_buf.trim_end();

        // Insert separator: newline unless the previous row wraps into this one.
        if let Some(pr) = prev_row {
            let wraps = read_cell_flags(term, Line(pr), last_col).contains(Flags::WRAPLINE);
            if !wraps {
                out.push('\n');
            }
        }
        out.push_str(trimmed);
        prev_row = Some(row);

        row = row.saturating_add(1);
    }

    out
}

/// Return whether `c` is a word character for double-click word selection.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | '~' | '@' | '+' | ':' | '%' | '#' | '?' | '&' | '=')
}

/// Find the word boundaries around `point` on the terminal grid.
///
/// If the character at `point` is a delimiter, both bounds equal `point`
/// (single-cell selection).  Returns `(start, end)` in reading order.
pub fn word_bounds_at(
    term: &Term<VoidListener>,
    point: SelectionPoint,
) -> (SelectionPoint, SelectionPoint) {
    let cols = term.grid().columns();
    let max_col = cols.saturating_sub(1);
    let point = SelectionPoint { row: point.row, col: point.col.min(max_col) };
    let line = Line(point.row);
    let c = read_cell_char(term, line, Column(point.col));
    if !is_word_char(c) {
        return (point, point);
    }

    // Scan left for word start.
    let mut start = point;
    while let Some(prev) = previous_cell_point(term, start) {
        if !is_word_char(read_cell_char(term, Line(prev.row), Column(prev.col))) {
            break;
        }
        start = prev;
    }

    // Scan right for word end.
    let mut end = point;
    while let Some(next) = next_cell_point(term, end) {
        if !is_word_char(read_cell_char(term, Line(next.row), Column(next.col))) {
            break;
        }
        end = next;
    }

    (start, end)
}

/// Return the start and end of the full logical line at `row`, spanning any
/// WRAPLINE-connected screen rows.
pub fn line_bounds_at(term: &Term<VoidListener>, row: i32) -> (SelectionPoint, SelectionPoint) {
    let logical = logical_line_at(term, row);
    let last_col = term.grid().columns().saturating_sub(1);
    (
        SelectionPoint { row: logical.first, col: 0 },
        SelectionPoint { row: logical.last, col: last_col },
    )
}

/// Extend a word-mode selection during double-click drag.
///
/// `anchor_start` and `anchor_end` are the word bounds from the initial
/// double-click.  `new_point` is the current drag position.
pub fn extend_by_word(
    term: &Term<VoidListener>,
    anchor_start: SelectionPoint,
    anchor_end: SelectionPoint,
    new_point: SelectionPoint,
) -> SelectionRange {
    let after_end = new_point > anchor_end;
    let before_start = new_point < anchor_start;

    if after_end {
        let (_, word_end) = word_bounds_at(term, new_point);
        SelectionRange::word(anchor_start, word_end)
    } else if before_start {
        let (word_start, _) = word_bounds_at(term, new_point);
        SelectionRange::word(word_start, anchor_end)
    } else {
        SelectionRange::word(anchor_start, anchor_end)
    }
}

/// Extend a line-mode selection during triple-click drag.
///
/// `anchor_start` and `anchor_end` are the line bounds from the initial
/// triple-click.  `new_point` is the current drag position.
pub fn extend_by_line(
    term: &Term<VoidListener>,
    anchor_start: SelectionPoint,
    anchor_end: SelectionPoint,
    new_point: SelectionPoint,
) -> SelectionRange {
    let after_end = new_point > anchor_end;
    let before_start = new_point < anchor_start;

    if after_end {
        let (_, drag_line_end) = line_bounds_at(term, new_point.row);
        SelectionRange::line(anchor_start, drag_line_end)
    } else if before_start {
        let (drag_line_start, _) = line_bounds_at(term, new_point.row);
        SelectionRange::line(drag_line_start, anchor_end)
    } else {
        SelectionRange::line(anchor_start, anchor_end)
    }
}

/// Return a reference to a single cell from the terminal grid.
///
/// `alacritty_terminal`'s `Grid` and `Row` only implement the `Index` trait
/// with no fallible `.get()` alternative, so indexing is required here —
/// matching the direct grid indexing the display snapshot path already relies
/// on.
fn read_cell(term: &Term<VoidListener>, line: Line, col: Column) -> &Cell {
    &term.grid()[line][col]
}

/// Read a single cell character from the terminal grid.
pub fn read_cell_char(term: &Term<VoidListener>, line: Line, col: Column) -> char {
    read_cell(term, line, col).c
}

/// Read the flags of a single cell from the terminal grid.
pub fn read_cell_flags(term: &Term<VoidListener>, line: Line, col: Column) -> Flags {
    read_cell(term, line, col).flags
}

/// One run of selected cells on a single *visible* row, ready to paint.
///
/// Columns are inclusive on both ends, matching the find overlay's
/// [`crate::search::MatchHighlight`], so the paint path treats a selection run
/// and a match run identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionSpan {
    /// Row index into the painted viewport (0 = top row on screen).
    pub row: usize,
    /// First selected column, inclusive.
    pub start_col: usize,
    /// Last selected column, inclusive.
    pub end_col: usize,
}

/// Project a selection onto the painted viewport as one span per visible row.
///
/// `range` is in absolute grid lines (0 = viewport top at offset 0, negative =
/// scrollback), so a viewport scrolled `display_offset` rows into the
/// scrollback shows grid line `row - display_offset` at screen row `row`. Rows
/// outside the `rows` x `cols` viewport are dropped rather than clamped: a
/// selection that scrolled off screen must paint nothing, not a stripe at the
/// edge. Returns no spans for an empty selection.
#[must_use]
pub fn viewport_spans(
    range: &SelectionRange,
    display_offset: usize,
    rows: usize,
    cols: usize,
) -> Vec<SelectionSpan> {
    if range.is_empty() || rows == 0 || cols == 0 {
        return Vec::new();
    }
    let (lo, hi) = range.normalized();
    let offset = selection_grid_i32(display_offset);
    let last_col = cols.saturating_sub(1);
    let mut spans = Vec::new();
    for row in lo.row..=hi.row {
        let screen = row + offset;
        if screen < 0 {
            continue;
        }
        let Ok(screen) = usize::try_from(screen) else { continue };
        if screen >= rows {
            break;
        }
        let start_col = if row == lo.row { lo.col } else { 0 };
        let end_col = if row == hi.row { hi.col } else { last_col };
        if start_col > last_col {
            continue;
        }
        spans.push(SelectionSpan { row: screen, start_col, end_col: end_col.min(last_col) });
    }
    spans
}

/// Return the previous logical neighbor for word scanning, crossing into the
/// wrapped row above when the current row is a continuation.
fn previous_cell_point(term: &Term<VoidListener>, point: SelectionPoint) -> Option<SelectionPoint> {
    if point.col > 0 {
        return Some(SelectionPoint { row: point.row, col: point.col.saturating_sub(1) });
    }

    let topmost = term.grid().topmost_line().0;
    if point.row <= topmost {
        return None;
    }

    let last_col = term.grid().columns().saturating_sub(1);
    let row_above = point.row - 1;
    if read_cell_flags(term, Line(row_above), Column(last_col)).contains(Flags::WRAPLINE) {
        Some(SelectionPoint { row: row_above, col: last_col })
    } else {
        None
    }
}

/// Return the next logical neighbor for word scanning, crossing into the
/// wrapped continuation row when the current row ends with WRAPLINE.
fn next_cell_point(term: &Term<VoidListener>, point: SelectionPoint) -> Option<SelectionPoint> {
    let last_col = term.grid().columns().saturating_sub(1);
    if point.col < last_col {
        return Some(SelectionPoint { row: point.row, col: point.col.saturating_add(1) });
    }

    let bottommost = term.grid().bottommost_line().0;
    if point.row >= bottommost {
        return None;
    }

    if read_cell_flags(term, Line(point.row), Column(last_col)).contains(Flags::WRAPLINE) {
        Some(SelectionPoint { row: point.row + 1, col: 0 })
    } else {
        None
    }
}

/// The row extent of a wrapped logical line.
#[derive(Debug, Clone, Copy)]
struct LogicalLine {
    first: i32,
    last: i32,
}

/// Find the full extent of the logical line that contains `row`, following
/// WRAPLINE flags to join screen rows that belong to the same logical line.
fn logical_line_at(term: &Term<VoidListener>, row: i32) -> LogicalLine {
    let topmost = term.grid().topmost_line().0;
    let bottommost = term.grid().bottommost_line().0;
    let last_col = Column(term.grid().columns().saturating_sub(1));

    // Scan upward: row_above wraps into row_above+1 when it has WRAPLINE set.
    let mut first = row;
    while first > topmost {
        let above = first - 1;
        if read_cell_flags(term, Line(above), last_col).contains(Flags::WRAPLINE) {
            first = above;
        } else {
            break;
        }
    }

    // Scan downward: current row wraps into current+1 when it has WRAPLINE set.
    let mut last = row;
    while last < bottommost {
        if read_cell_flags(term, Line(last), last_col).contains(Flags::WRAPLINE) {
            last += 1;
        } else {
            break;
        }
    }

    LogicalLine { first, last }
}

/// Interactive mouse-drag selection state with copy-on-select.
///
/// Tracks the active [`SelectionRange`] plus the word/line anchors captured on
/// the initial double/triple click, so a subsequent drag extends whole words or
/// lines rather than individual cells. [`SelectionState::copy_text`] yields the
/// selected text for copy-on-select the moment a drag settles.
#[derive(Debug, Clone, Default)]
pub struct SelectionState {
    range: Option<SelectionRange>,
    word_anchor: Option<(SelectionPoint, SelectionPoint)>,
    line_anchor: Option<(SelectionPoint, SelectionPoint)>,
}

impl SelectionState {
    /// Create an empty selection state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a cell-granularity selection at `point` (single click).
    pub fn start_cell(&mut self, point: SelectionPoint) {
        self.word_anchor = None;
        self.line_anchor = None;
        self.range = Some(SelectionRange::cell(point, point));
    }

    /// Begin a word-granularity selection around `point` (double click).
    pub fn start_word(&mut self, term: &Term<VoidListener>, point: SelectionPoint) {
        let (start, end) = word_bounds_at(term, point);
        self.line_anchor = None;
        self.word_anchor = Some((start, end));
        self.range = Some(SelectionRange::word(start, end));
    }

    /// Begin a line-granularity selection over `point`'s logical line (triple
    /// click).
    pub fn start_line(&mut self, term: &Term<VoidListener>, point: SelectionPoint) {
        let (start, end) = line_bounds_at(term, point.row);
        self.word_anchor = None;
        self.line_anchor = Some((start, end));
        self.range = Some(SelectionRange::line(start, end));
    }

    /// Extend the active selection to `point` as the pointer drags. The
    /// granularity matches whichever `start_*` began the gesture.
    pub fn drag_to(&mut self, term: &Term<VoidListener>, point: SelectionPoint) {
        let Some(range) = self.range else {
            return;
        };
        self.range = Some(match range.mode {
            SelectionMode::Cell => SelectionRange::cell(range.start, point),
            SelectionMode::Word => {
                let (anchor_start, anchor_end) =
                    self.word_anchor.unwrap_or((range.start, range.end));
                extend_by_word(term, anchor_start, anchor_end, point)
            }
            SelectionMode::Line => {
                let (anchor_start, anchor_end) =
                    self.line_anchor.unwrap_or((range.start, range.end));
                extend_by_line(term, anchor_start, anchor_end, point)
            }
        });
    }

    /// The active selection range, if any.
    pub fn range(&self) -> Option<SelectionRange> {
        self.range
    }

    /// Extract the selected text for copy-on-select. Returns `None` when there
    /// is no selection or the selection is empty.
    pub fn copy_text(&self, term: &Term<VoidListener>) -> Option<String> {
        let range = self.range?;
        if range.is_empty() {
            return None;
        }
        Some(extract_text(term, &range))
    }

    /// Clear the active selection and any drag anchors.
    pub fn clear(&mut self) {
        self.range = None;
        self.word_anchor = None;
        self.line_anchor = None;
    }

    /// Shift the active selection and anchors by `delta` grid lines, keeping the
    /// selection pinned to content as scrollback is trimmed.
    pub fn shift_rows(&mut self, delta: i32) {
        if let Some(range) = self.range.as_mut() {
            range.shift_rows(delta);
        }
        if let Some((start, end)) = self.word_anchor.as_mut() {
            start.shift_row(delta);
            end.shift_row(delta);
        }
        if let Some((start, end)) = self.line_anchor.as_mut() {
            start.shift_row(delta);
            end.shift_row(delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal_gpui::event::VoidListener;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::{Config, Term};
    use vte::ansi::Processor;

    use super::{
        SelectionMode, SelectionPoint, SelectionRange, SelectionSpan, SelectionState, extract_text,
        line_bounds_at, viewport_spans, word_bounds_at,
    };

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

    fn point(row: i32, col: usize) -> SelectionPoint {
        SelectionPoint { row, col }
    }

    // @lat: [[test#GPUI Terminal Selection#Cell selection extracts a substring]]
    #[gpui::test]
    fn cell_selection_extracts_substring() {
        let term = term_with_output(20, 3, b"hello world");
        let range = SelectionRange::cell(point(0, 0), point(0, 4));
        assert_eq!(extract_text(&term, &range), "hello");
    }

    // @lat: [[test#GPUI Terminal Selection#Reversed cell selection normalizes]]
    #[gpui::test]
    fn reversed_cell_selection_normalizes() {
        let term = term_with_output(20, 3, b"hello world");
        let forward = SelectionRange::cell(point(0, 6), point(0, 10));
        let reversed = SelectionRange::cell(point(0, 10), point(0, 6));
        assert_eq!(extract_text(&term, &forward), "world");
        assert_eq!(extract_text(&term, &reversed), "world");
    }

    // @lat: [[test#GPUI Terminal Selection#Word bounds snap to word characters]]
    #[gpui::test]
    fn word_bounds_snap_to_word_characters() {
        let term = term_with_output(30, 3, b"alpha beta_gamma delta");
        // Cursor inside "beta_gamma" (underscore is a word char).
        let (start, end) = word_bounds_at(&term, point(0, 8));
        assert_eq!(start, point(0, 6));
        assert_eq!(end, point(0, 15));
        let range = SelectionRange::word(start, end);
        assert_eq!(extract_text(&term, &range), "beta_gamma");
    }

    // @lat: [[test#GPUI Terminal Selection#Word bounds on a delimiter select one cell]]
    #[gpui::test]
    fn word_bounds_on_delimiter_select_single_cell() {
        let term = term_with_output(30, 3, b"alpha beta");
        // Column 5 is the space delimiter.
        let (start, end) = word_bounds_at(&term, point(0, 5));
        assert_eq!(start, point(0, 5));
        assert_eq!(end, point(0, 5));
    }

    // @lat: [[test#GPUI Terminal Selection#Line bounds span the full row]]
    #[gpui::test]
    fn line_bounds_span_full_row() {
        let term = term_with_output(12, 3, b"hi");
        let (start, end) = line_bounds_at(&term, 0);
        assert_eq!(start, point(0, 0));
        assert_eq!(end, point(0, 11));
    }

    // @lat: [[test#GPUI Terminal Selection#WRAPLINE joins a wrapped row without a newline]]
    #[gpui::test]
    fn wrapline_joins_wrapped_row_without_newline() {
        // Ten columns; twelve chars force an autowrap so row 0 ends WRAPLINE.
        let term = term_with_output(10, 4, b"abcdefghijKL");
        let range = SelectionRange::cell(point(0, 0), point(1, 1));
        assert_eq!(extract_text(&term, &range), "abcdefghijKL");
    }

    // @lat: [[test#GPUI Terminal Selection#Hard line break inserts a newline]]
    #[gpui::test]
    fn hard_break_inserts_newline() {
        let term = term_with_output(20, 4, b"first\r\nsecond");
        let range = SelectionRange::cell(point(0, 0), point(1, 5));
        assert_eq!(extract_text(&term, &range), "first\nsecond");
    }

    // @lat: [[test#GPUI Terminal Selection#Word bounds follow a wrapped line]]
    #[gpui::test]
    fn word_bounds_follow_wrapped_line() {
        // "abcdefghij_word" wraps at 10 cols; the word crosses the WRAPLINE.
        let term = term_with_output(10, 4, b"abcdefghij_word");
        let (start, end) = word_bounds_at(&term, point(1, 2));
        assert_eq!(start, point(0, 0));
        assert_eq!(end, point(1, 4));
        let range = SelectionRange::word(start, end);
        assert_eq!(extract_text(&term, &range), "abcdefghij_word");
    }

    // @lat: [[test#GPUI Terminal Selection#Line bounds span a wrapped logical line]]
    #[gpui::test]
    fn line_bounds_span_wrapped_logical_line() {
        let term = term_with_output(10, 4, b"abcdefghijKL");
        let (start, end) = line_bounds_at(&term, 1);
        assert_eq!(start, point(0, 0));
        assert_eq!(end, point(1, 9));
    }

    // @lat: [[test#GPUI Terminal Selection#Contains-cell honors selection shape]]
    #[gpui::test]
    fn contains_cell_honors_selection_shape() {
        let range = SelectionRange::cell(point(0, 3), point(2, 4));
        assert!(!range.contains_cell(0, 2));
        assert!(range.contains_cell(0, 3));
        assert!(range.contains_cell(1, 0));
        assert!(range.contains_cell(2, 4));
        assert!(!range.contains_cell(2, 5));
    }

    // @lat: [[test#GPUI Terminal Selection#Selection state copies on select]]
    #[gpui::test]
    fn selection_state_copies_on_select() {
        let term = term_with_output(30, 3, b"alpha beta_gamma delta");
        let mut state = SelectionState::new();

        // Cell drag.
        state.start_cell(point(0, 0));
        state.drag_to(&term, point(0, 4));
        assert_eq!(state.copy_text(&term).as_deref(), Some("alpha"));

        // Word double-click snaps and copies the whole word.
        state.start_word(&term, point(0, 8));
        assert_eq!(state.range().map(|r| r.mode), Some(SelectionMode::Word));
        assert_eq!(state.copy_text(&term).as_deref(), Some("beta_gamma"));

        // Line triple-click copies the full row.
        state.start_line(&term, point(0, 0));
        assert_eq!(state.range().map(|r| r.mode), Some(SelectionMode::Line));
        assert_eq!(state.copy_text(&term).as_deref(), Some("alpha beta_gamma delta"));

        // Empty selection yields nothing.
        state.start_cell(point(0, 2));
        assert_eq!(state.copy_text(&term), None);
        state.clear();
        assert_eq!(state.range(), None);
    }

    // @lat: [[test#GPUI Terminal Selection#Word drag extends by whole words]]
    #[gpui::test]
    fn word_drag_extends_by_whole_words() {
        let term = term_with_output(30, 3, b"alpha beta gamma");
        let mut state = SelectionState::new();
        state.start_word(&term, point(0, 1)); // "alpha"
        state.drag_to(&term, point(0, 12)); // into "gamma"
        assert_eq!(state.copy_text(&term).as_deref(), Some("alpha beta gamma"));
    }

    // @lat: [[test#GPUI Terminal Selection#Selection projects onto visible rows]]
    #[test]
    fn selection_projects_one_span_per_visible_row() {
        // Rows 0..=2 of a 5-row, 10-column viewport at offset 0.
        let range = SelectionRange::cell(point(0, 3), point(2, 4));
        assert_eq!(
            viewport_spans(&range, 0, 5, 10),
            vec![
                SelectionSpan { row: 0, start_col: 3, end_col: 9 },
                SelectionSpan { row: 1, start_col: 0, end_col: 9 },
                SelectionSpan { row: 2, start_col: 0, end_col: 4 },
            ]
        );
    }

    // @lat: [[test#GPUI Terminal Selection#Scrollback selection follows the offset]]
    #[test]
    fn scrollback_selection_moves_with_the_display_offset() {
        // A selection two lines into the scrollback paints nothing at offset 0
        // and lands on the top rows once the viewport is scrolled onto it.
        let range = SelectionRange::cell(point(-2, 0), point(-2, 3));
        assert!(viewport_spans(&range, 0, 5, 10).is_empty());
        assert_eq!(
            viewport_spans(&range, 2, 5, 10),
            vec![SelectionSpan { row: 0, start_col: 0, end_col: 3 }]
        );
    }

    // @lat: [[test#GPUI Terminal Selection#Empty selection paints nothing]]
    #[test]
    fn empty_selection_and_empty_viewport_paint_nothing() {
        let empty = SelectionRange::cell(point(1, 4), point(1, 4));
        assert!(viewport_spans(&empty, 0, 5, 10).is_empty());
        let real = SelectionRange::cell(point(0, 0), point(0, 4));
        assert!(viewport_spans(&real, 0, 0, 10).is_empty());
        assert!(viewport_spans(&real, 0, 5, 0).is_empty());
    }
}
