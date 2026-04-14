//! Text selection: grid coordinate mapping and text extraction.
//!
//! Provides types for tracking a selection range on the terminal grid,
//! converting pixel coordinates to grid cells, and extracting selected
//! text from the terminal emulator state.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

use scribe_common::config::ContentPadding;

use crate::layout::Rect;
use crate::mouse_state::SelectionMode;

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

#[derive(Clone, Copy)]
pub struct PixelToGridRequest<'a> {
    pub x: f32,
    pub y: f32,
    pub pane_rect: Rect,
    pub cell_size: (f32, f32),
    pub tab_bar_height: f32,
    pub prompt_bar_height: f32,
    pub prompt_bar_at_top: bool,
    pub display_offset: usize,
    pub padding: &'a ContentPadding,
}

#[derive(Clone, Copy)]
enum ContentBoundsMode {
    RejectOutsideContent,
    ClampToContent,
}

const MAX_SELECTION_GRID_UNITS: usize = 65_535;

fn selection_grid_units(units: usize) -> u16 {
    u16::try_from(units.min(MAX_SELECTION_GRID_UNITS)).unwrap_or(u16::MAX)
}

fn selection_grid_pixels(units: usize, cell_size: f32) -> f32 {
    f32::from(selection_grid_units(units)) * cell_size
}

fn selection_units_in_extent(extent: f32, cell_size: f32) -> usize {
    if cell_size <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 0;
    }

    let mut low = 0usize;
    let mut high = 1usize;
    while high < MAX_SELECTION_GRID_UNITS && selection_grid_pixels(high, cell_size) <= extent {
        low = high;
        high = high.saturating_mul(2).min(MAX_SELECTION_GRID_UNITS);
        if high == low {
            break;
        }
    }

    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if selection_grid_pixels(mid, cell_size) <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low
}

fn selection_grid_i32(units: usize) -> i32 {
    i32::try_from(units).unwrap_or(i32::MAX)
}

/// Convert pixel coordinates to an absolute grid position within a pane.
///
/// The content area excludes the tab bar at the top and the status bar at the
/// bottom.  Returns `None` when the pixel position falls outside the content
/// area.
///
/// The returned `row` is an **absolute grid line** (matching
/// `alacritty_terminal`'s `Line` index): 0 is the top of the current
/// viewport, negative values point into scrollback history.  The
/// `display_offset` parameter is subtracted from the screen row so that
/// the selection tracks content rather than screen position.
pub fn pixel_to_grid(request: PixelToGridRequest<'_>) -> Option<SelectionPoint> {
    pixel_to_grid_impl(request, ContentBoundsMode::RejectOutsideContent)
}

/// Convert pixel coordinates to an absolute grid position, clamping points
/// outside the content area to the nearest visible cell.
pub fn pixel_to_grid_clamped(request: PixelToGridRequest<'_>) -> Option<SelectionPoint> {
    pixel_to_grid_impl(request, ContentBoundsMode::ClampToContent)
}

fn pixel_to_grid_impl(
    request: PixelToGridRequest<'_>,
    bounds_mode: ContentBoundsMode,
) -> Option<SelectionPoint> {
    let (cell_w, cell_h) = request.cell_size;
    let chrome_above = request.tab_bar_height
        + if request.prompt_bar_at_top { request.prompt_bar_height } else { 0.0 };
    let total_chrome = request.tab_bar_height + request.prompt_bar_height;
    let content_x = request.pane_rect.x + request.padding.left;
    let content_y = request.pane_rect.y + chrome_above + request.padding.top;
    let content_w =
        (request.pane_rect.width - request.padding.left - request.padding.right).max(0.0);
    let content_h =
        (request.pane_rect.height - total_chrome - request.padding.top - request.padding.bottom)
            .max(0.0);

    if content_w <= 0.0 || content_h <= 0.0 {
        return None;
    }

    // Pixel offset relative to the content area origin.
    let raw_rel_x = request.x - content_x;
    let raw_rel_y = request.y - content_y;

    // Reject clicks outside the content area.
    if matches!(bounds_mode, ContentBoundsMode::RejectOutsideContent)
        && (raw_rel_x < 0.0 || raw_rel_y < 0.0 || raw_rel_x >= content_w || raw_rel_y >= content_h)
    {
        return None;
    }

    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    let rel_x = if matches!(bounds_mode, ContentBoundsMode::ClampToContent) {
        raw_rel_x.clamp(0.0, (content_w - f32::EPSILON).max(0.0))
    } else {
        raw_rel_x
    };
    let rel_y = if matches!(bounds_mode, ContentBoundsMode::ClampToContent) {
        raw_rel_y.clamp(0.0, (content_h - f32::EPSILON).max(0.0))
    } else {
        raw_rel_y
    };

    // Clamp to the valid grid range — the content area may contain a
    // fractional cell at the right/bottom edge, so `floor(content / cell)`
    // could exceed the last valid index.
    let max_col = selection_units_in_extent(content_w, cell_w);
    let max_row = selection_grid_i32(selection_units_in_extent(content_h, cell_h));
    let col = selection_units_in_extent(rel_x, cell_w).min(max_col.saturating_sub(1));
    let screen_row =
        selection_grid_i32(selection_units_in_extent(rel_y, cell_h)).min(max_row.saturating_sub(1));

    // Convert screen row to absolute grid line: subtract display_offset so
    // that scrollback lines get negative indices matching alacritty_terminal.
    let row = screen_row.saturating_sub(selection_grid_i32(request.display_offset));

    Some(SelectionPoint { row, col })
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
/// with no fallible `.get()` alternative, so we must use indexing here.
fn read_cell(term: &Term<VoidListener>, line: Line, col: Column) -> &Cell {
    #[allow(
        clippy::indexing_slicing,
        reason = "alacritty_terminal grid only supports Index trait, no get() alternative"
    )]
    &term.grid()[line][col]
}

/// Read a single cell character from the terminal grid.
pub fn read_cell_char(term: &Term<VoidListener>, line: Line, col: Column) -> char {
    read_cell(term, line, col).c
}

/// Read the flags of a single cell from the terminal grid.
fn read_cell_flags(term: &Term<VoidListener>, line: Line, col: Column) -> Flags {
    read_cell(term, line, col).flags
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
