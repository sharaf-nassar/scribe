//! Text selection: grid coordinate mapping and text extraction.
//!
//! Provides types for tracking a selection range on the terminal grid,
//! converting pixel coordinates to grid cells, and extracting selected
//! text from the terminal emulator state.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};

use crate::layout::Rect;
use crate::pane::TAB_BAR_HEIGHT;

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
}

impl SelectionRange {
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
    #[allow(dead_code, reason = "will be used for selection highlight rendering")]
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

/// Convert pixel coordinates to a grid position within a pane's content area.
///
/// The content area excludes the tab bar at the top and the status bar at the
/// bottom.  Returns `None` when the pixel position falls outside the content
/// area.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel / cell_size yields a small positive value fitting in usize / i32"
)]
pub fn pixel_to_grid(
    x: f32,
    y: f32,
    pane_rect: Rect,
    cell_w: f32,
    cell_h: f32,
) -> Option<SelectionPoint> {
    let content_x = pane_rect.x;
    let content_y = pane_rect.y + TAB_BAR_HEIGHT;
    let content_w = pane_rect.width;
    let content_h = (pane_rect.height - TAB_BAR_HEIGHT).max(0.0);

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
    let row = ((rel_y / cell_h) as i32).min(max_row.saturating_sub(1));

    Some(SelectionPoint { row, col })
}

/// Extract the selected text from the terminal grid.
///
/// Walks rows from the normalised start to the normalised end, collecting
/// cell characters.  Trailing spaces on each row are trimmed, and rows are
/// joined with `'\n'`.
pub fn extract_text(term: &Term<VoidListener>, range: &SelectionRange) -> String {
    let (lo, hi) = range.normalized();

    let screen_lines = term.grid().screen_lines();
    let cols = term.grid().columns();

    let mut lines: Vec<String> = Vec::new();

    let mut row = lo.row;
    while row <= hi.row {
        let line_obj = Line(row_to_alacritty_line(row, screen_lines));

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

/// Map a zero-based row index to the `alacritty_terminal` `Line` inner value.
///
/// `alacritty_terminal` uses 0 for the top visible line and positive values
/// going down, which matches our zero-based row directly.
const fn row_to_alacritty_line(row: i32, _screen_lines: usize) -> i32 {
    row
}

/// Read a single cell character from the terminal grid.
///
/// `alacritty_terminal`'s `Grid` and `Row` only implement the `Index` trait
/// with no fallible `.get()` alternative, so we must use indexing here.
fn read_cell_char(term: &Term<VoidListener>, line: Line, col: Column) -> char {
    #[allow(
        clippy::indexing_slicing,
        reason = "alacritty_terminal grid only supports Index trait, no get() alternative"
    )]
    let cell = &term.grid()[line][col];
    cell.c
}
