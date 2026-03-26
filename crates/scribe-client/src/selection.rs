//! Text selection: grid coordinate mapping and text extraction.
//!
//! Provides types for tracking a selection range on the terminal grid,
//! converting pixel coordinates to grid cells, and extracting selected
//! text from the terminal emulator state.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};

use scribe_common::config::ContentPadding;

use crate::layout::Rect;
use crate::mouse_state::SelectionMode;

/// A position on the terminal grid, in row/column coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: i32,
    pub col: usize,
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
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel / cell_size yields a small positive value fitting in usize / i32"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "coordinate conversion needs pane geometry, cell size, tab bar height, and scroll offset"
)]
pub fn pixel_to_grid(
    x: f32,
    y: f32,
    pane_rect: Rect,
    cell_w: f32,
    cell_h: f32,
    tab_bar_height: f32,
    display_offset: usize,
    padding: &ContentPadding,
) -> Option<SelectionPoint> {
    let content_x = pane_rect.x + padding.left;
    let content_y = pane_rect.y + tab_bar_height + padding.top;
    let content_w = (pane_rect.width - padding.left - padding.right).max(0.0);
    let content_h = (pane_rect.height - tab_bar_height - padding.top - padding.bottom).max(0.0);

    // Pixel offset relative to the content area origin.
    let rel_x = x - content_x;
    let rel_y = y - content_y;

    // Reject clicks outside the content area.
    if rel_x < 0.0 || rel_y < 0.0 || rel_x >= content_w || rel_y >= content_h {
        return None;
    }

    if cell_w <= 0.0 || cell_h <= 0.0 {
        return None;
    }

    // Clamp to the valid grid range — the content area may contain a
    // fractional cell at the right/bottom edge, so `floor(content / cell)`
    // could exceed the last valid index.
    let max_col = (content_w / cell_w) as usize;
    let max_row = (content_h / cell_h) as i32;
    let col = ((rel_x / cell_w) as usize).min(max_col.saturating_sub(1));
    let screen_row = ((rel_y / cell_h) as i32).min(max_row.saturating_sub(1));

    // Convert screen row to absolute grid line: subtract display_offset so
    // that scrollback lines get negative indices matching alacritty_terminal.
    #[allow(
        clippy::cast_possible_wrap,
        reason = "display_offset is bounded by scrollback_lines (≤ 100_000), fits in i32"
    )]
    let row = screen_row - display_offset as i32;

    Some(SelectionPoint { row, col })
}

/// Extract the selected text from the terminal grid.
///
/// Selection rows are **absolute grid lines** (0 = viewport top, negative =
/// scrollback), matching the `Line` index used by `alacritty_terminal`.
/// Walks rows from the normalised start to the normalised end, collecting
/// cell characters.  Trailing spaces on each row are trimmed, and rows are
/// joined with `'\n'`.
pub fn extract_text(term: &Term<VoidListener>, range: &SelectionRange) -> String {
    let (lo, hi) = range.normalized();

    let cols = term.grid().columns();

    let mut lines: Vec<String> = Vec::new();

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

        // Trim trailing spaces from this row.
        let trimmed = line_buf.trim_end();
        lines.push(trimmed.to_owned());

        row = row.saturating_add(1);
    }

    lines.join("\n")
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
#[allow(
    clippy::cast_possible_wrap,
    reason = "column index is bounded by terminal width which fits in i32"
)]
pub fn word_bounds_at(
    term: &Term<VoidListener>,
    point: SelectionPoint,
) -> (SelectionPoint, SelectionPoint) {
    let cols = term.grid().columns();
    let max_col = cols.saturating_sub(1);
    let line = Line(point.row);
    let c = read_cell_char(term, line, Column(point.col.min(max_col)));
    if !is_word_char(c) {
        return (point, point);
    }

    // Scan left for word start.
    let mut start_col = point.col;
    while start_col > 0 {
        let prev = start_col.saturating_sub(1);
        if !is_word_char(read_cell_char(term, line, Column(prev))) {
            break;
        }
        start_col = prev;
    }

    // Scan right for word end.
    let mut end_col = point.col;
    while end_col < max_col {
        let next = end_col.saturating_add(1);
        if !is_word_char(read_cell_char(term, line, Column(next))) {
            break;
        }
        end_col = next;
    }

    (
        SelectionPoint { row: point.row, col: start_col },
        SelectionPoint { row: point.row, col: end_col },
    )
}

/// Return the start and end of the full line at `row`.
pub fn line_bounds_at(term: &Term<VoidListener>, row: i32) -> (SelectionPoint, SelectionPoint) {
    let last_col = term.grid().columns().saturating_sub(1);
    (SelectionPoint { row, col: 0 }, SelectionPoint { row, col: last_col })
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
    let after_end = new_point.row > anchor_end.row
        || (new_point.row == anchor_end.row && new_point.col > anchor_end.col);
    let before_start = new_point.row < anchor_start.row
        || (new_point.row == anchor_start.row && new_point.col < anchor_start.col);

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

/// Read a single cell character from the terminal grid.
///
/// `alacritty_terminal`'s `Grid` and `Row` only implement the `Index` trait
/// with no fallible `.get()` alternative, so we must use indexing here.
pub fn read_cell_char(term: &Term<VoidListener>, line: Line, col: Column) -> char {
    #[allow(
        clippy::indexing_slicing,
        reason = "alacritty_terminal grid only supports Index trait, no get() alternative"
    )]
    let cell = &term.grid()[line][col];
    cell.c
}
