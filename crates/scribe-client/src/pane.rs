//! Per-pane state: terminal emulator and ANSI processor.
//!
//! Each pane owns a [`Term`] and a VTE [`Processor`]. Rendering is
//! performed by the shared [`TerminalRenderer`] in `GpuContext`.

use std::path::PathBuf;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_renderer::types::GridSize;

use crate::layout::Rect;
use crate::scrollbar::ScrollbarState;

/// State for a single terminal pane.
pub struct Pane {
    pub session_id: SessionId,
    #[allow(dead_code, reason = "used by tab bar rendering and workspace management")]
    pub workspace_id: WorkspaceId,
    #[allow(dead_code, reason = "used by tab bar text rendering")]
    pub workspace_name: Option<String>,
    #[allow(dead_code, reason = "used by tab bar text rendering")]
    pub title: String,
    /// Current working directory reported by the shell via OSC 7.
    pub cwd: Option<PathBuf>,
    /// Current git branch name (or short SHA in detached HEAD).
    pub git_branch: Option<String>,
    pub term: Term<VoidListener>,
    pub ansi_processor: vte::ansi::Processor,
    /// The most recently assigned pixel rect from the layout engine.
    pub rect: Rect,
    /// Grid size (cols, rows) for this pane's content area.
    pub grid: GridSize,
    #[allow(
        dead_code,
        reason = "read by scrollbar rendering and hit-testing, wired in later tasks"
    )]
    pub scrollbar_state: ScrollbarState,
}

/// Simple adapter implementing `alacritty_terminal::grid::Dimensions`.
struct TermDims {
    cols: usize,
    lines: usize,
}

impl alacritty_terminal::grid::Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl Pane {
    /// Create a new pane with its own terminal.
    ///
    /// `rect` is the pixel area assigned to this pane by the layout engine.
    /// `grid` is the initial grid size computed from the content area.
    pub fn new(
        rect: Rect,
        grid: GridSize,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Self {
        let dims = TermDims { cols: usize::from(grid.cols), lines: usize::from(grid.rows) };
        let term = Term::new(alacritty_terminal::term::Config::default(), &dims, VoidListener);

        Self {
            session_id,
            workspace_id,
            workspace_name: None,
            title: String::from("shell"),
            cwd: None,
            git_branch: None,
            term,
            ansi_processor: vte::ansi::Processor::new(),
            rect,
            grid,
            scrollbar_state: ScrollbarState::new(),
        }
    }

    /// Feed raw PTY output bytes into the ANSI processor / terminal.
    pub fn feed_output(&mut self, bytes: &[u8]) {
        self.ansi_processor.advance(&mut self.term, bytes);
    }

    /// Resize just the underlying terminal emulator without changing the
    /// pane's stored `rect` or `grid`.
    ///
    /// Used during snapshot restoration: the ANSI content must be processed
    /// in a term whose dimensions match the snapshot, then resized back to
    /// the pane's actual grid so `alacritty_terminal` reflows correctly.
    pub fn resize_term_only(&mut self, cols: u16, rows: u16) {
        let dims = TermDims { cols: usize::from(cols), lines: usize::from(rows) };
        if self.term.columns() != dims.cols || self.term.screen_lines() != dims.lines {
            self.term.resize(dims);
        }
    }

    /// Resize this pane to a new pixel rect.
    ///
    /// Returns the new grid dimensions (cols, rows) for sending to the server.
    pub fn resize(&mut self, new_rect: Rect, new_grid: GridSize) -> GridSize {
        self.rect = new_rect;
        self.grid = new_grid;

        let dims = TermDims { cols: usize::from(new_grid.cols), lines: usize::from(new_grid.rows) };
        if self.term.columns() != dims.cols || self.term.screen_lines() != dims.lines {
            self.term.resize(dims);
        }

        new_grid
    }

    /// Return the pixel offset where terminal content starts (below tab bar).
    pub fn content_offset(&self, tab_bar_height: f32) -> (f32, f32) {
        (self.rect.x, self.rect.y + tab_bar_height)
    }

    /// Return the content area (below tab bar) as a viewport size tuple.
    #[allow(dead_code, reason = "public API for pane viewport queries")]
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "pane rect dimensions are small non-negative pixel values that fit in u32"
    )]
    pub fn content_viewport(&self, tab_bar_height: f32) -> (u32, u32) {
        let h = (self.rect.height - tab_bar_height).max(1.0);
        (self.rect.width.max(1.0) as u32, h as u32)
    }
}

/// Compute the grid size for a pane's content area.
pub fn compute_pane_grid(
    rect: Rect,
    cell_width: f32,
    cell_height: f32,
    tab_bar_height: f32,
) -> GridSize {
    let content_w = rect.width;
    let content_h = (rect.height - tab_bar_height).max(1.0);
    grid_from_pixels(content_w, content_h, cell_width, cell_height)
}

/// Compute grid dimensions from pixel dimensions and cell size.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel / cell_size yields a small positive value fitting in u16"
)]
fn grid_from_pixels(width: f32, height: f32, cell_w: f32, cell_h: f32) -> GridSize {
    let cols = if cell_w > 0.0 { (width / cell_w) as u16 } else { 1 };
    let rows = if cell_h > 0.0 { (height / cell_h) as u16 } else { 1 };
    GridSize { cols: cols.max(1), rows: rows.max(1) }
}
