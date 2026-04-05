//! Per-pane state: terminal emulator and ANSI processor.
//!
//! Each pane owns a [`Term`] and a VTE [`Processor`]. Rendering is
//! performed by the shared [`TerminalRenderer`] in `GpuContext`.

use std::path::PathBuf;
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use scribe_common::config::ContentPadding;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::SessionContext;
use scribe_renderer::types::{CellInstance, GridSize};

use crate::layout::{PaneEdges, Rect};
use crate::restore_state::LaunchBinding;
use crate::scrollbar::ScrollbarState;
use crate::selection::SelectionRange;

/// State for a single terminal pane.
#[allow(clippy::struct_excessive_bools, reason = "pane has legitimate independent boolean flags")]
pub struct Pane {
    pub session_id: SessionId,
    pub launch_binding: LaunchBinding,
    #[allow(dead_code, reason = "used by tab bar rendering and workspace management")]
    pub workspace_id: WorkspaceId,
    #[allow(dead_code, reason = "used by tab bar text rendering")]
    pub workspace_name: Option<String>,
    #[allow(dead_code, reason = "used by tab bar text rendering")]
    pub title: String,
    pub shell_name: String,
    /// Preferred tab label while Codex is actively working on a task.
    pub codex_task_label: Option<String>,
    /// Current working directory reported by the shell via OSC 7.
    pub cwd: Option<PathBuf>,
    /// Current shell/session context (remote host and tmux session).
    pub session_context: Option<SessionContext>,
    /// Current git branch name (or short SHA in detached HEAD).
    pub git_branch: Option<String>,
    pub term: Term<VoidListener>,
    pub ansi_processor: vte::ansi::Processor,
    /// The most recently assigned pixel rect from the layout engine.
    pub rect: Rect,
    /// Grid size (cols, rows) for this pane's content area.
    pub grid: GridSize,
    /// Which edges of this pane border the viewport (vs. another pane).
    pub edges: PaneEdges,
    #[allow(
        dead_code,
        reason = "read by scrollbar rendering and hit-testing, wired in later tasks"
    )]
    pub scrollbar_state: ScrollbarState,
    /// Set to `true` whenever PTY output, resize, or scroll changes the
    /// terminal state. Cleared after the instances are rebuilt. Starts as
    /// `true` so the first frame always performs a full build.
    pub content_dirty: bool,
    /// Last-built cell instances for this pane, reused when `content_dirty`
    /// is false and rendering context (cursor, focus, selection) hasn't changed.
    pub last_instances: Vec<CellInstance>,
    /// The `cursor_visible` value used when `last_instances` was built.
    /// `None` means instances have never been built.
    pub last_cursor_visible: Option<bool>,
    /// Whether this pane was the focused pane when `last_instances` was built.
    /// `None` means instances have never been built.
    pub last_was_focused: Option<bool>,
    /// Whether the terminal's own cursor was hidden (DECTCEM off) when
    /// `last_instances` was built.  Ensures the cache invalidates when the
    /// program toggles cursor visibility without other content changes.
    pub last_term_cursor_hidden: Option<bool>,
    /// The selection rendered when `last_instances` was built, or `None`.
    pub last_selection: Option<SelectionRange>,
    /// The grid size last sent to the server via IPC resize.
    /// `None` means a resize has never been sent for this pane.
    pub last_sent_grid: Option<GridSize>,
    /// Absolute line positions where prompts start (OSC 133;A marks).
    /// Used for prompt jumping and scrollbar indicators. Stored as
    /// "lines from the very top of the scrollback" (0 = oldest line).
    pub prompt_marks: Vec<usize>,
    /// Whether the current prompt has `click_events=1` enabled (OSC 133;A).
    pub click_events: bool,
    /// Absolute position where the prompt input starts (OSC 133;B mark).
    /// `Some((absolute_line, column))` while waiting for user input.
    /// Cleared when a command starts or ends (OSC 133;C / D).
    pub input_start: Option<(usize, usize)>,
    /// Text of the first user prompt submitted in this AI session.
    #[allow(dead_code, reason = "read by prompt bar renderer in a later task")]
    pub first_prompt: Option<String>,
    /// Text of the most recent user prompt (differs from first after 2+ prompts).
    #[allow(dead_code, reason = "read by prompt bar renderer in a later task")]
    pub latest_prompt: Option<String>,
    /// Total number of prompts received in this session.
    pub prompt_count: u32,
    /// Last-seen `conversation_id` for detecting session resets.
    #[allow(dead_code, reason = "read by prompt bar renderer in a later task")]
    pub last_conversation_id: Option<String>,
    /// Whether the user has dismissed the prompt bar for this pane.
    /// Cleared when a new conversation starts or prompts are reset.
    pub prompt_bar_dismissed: bool,
}

/// Result of feeding PTY bytes into a pane's ANSI processor.
pub struct FeedOutputResult {
    /// `true` when the processed bytes changed visible terminal state and the
    /// pane should be re-rendered immediately.
    pub needs_redraw: bool,
    /// `true` when a synchronized update is still open and may require a
    /// timeout-based flush if no terminating `CSI ? 2026 l` arrives.
    pub sync_pending: bool,
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
    #[allow(
        clippy::too_many_arguments,
        reason = "pane construction already needs layout + launch binding"
    )]
    pub fn new(
        rect: Rect,
        grid: GridSize,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        edges: PaneEdges,
        launch_binding: LaunchBinding,
    ) -> Self {
        let dims = TermDims { cols: usize::from(grid.cols), lines: usize::from(grid.rows) };
        let term = Term::new(alacritty_terminal::term::Config::default(), &dims, VoidListener);

        Self {
            session_id,
            launch_binding,
            workspace_id,
            workspace_name: None,
            title: String::from("shell"),
            shell_name: String::from("shell"),
            codex_task_label: None,
            cwd: None,
            session_context: None,
            git_branch: None,
            term,
            ansi_processor: vte::ansi::Processor::new(),
            rect,
            grid,
            edges,
            scrollbar_state: ScrollbarState::new(),
            content_dirty: true,
            last_instances: Vec::new(),
            last_cursor_visible: None,
            last_term_cursor_hidden: None,
            last_was_focused: None,
            last_selection: None,
            last_sent_grid: None,
            prompt_marks: Vec::new(),
            click_events: false,
            input_start: None,
            first_prompt: None,
            latest_prompt: None,
            prompt_count: 0,
            last_conversation_id: None,
            prompt_bar_dismissed: false,
        }
    }

    /// Feed raw PTY output bytes into the ANSI processor / terminal.
    pub fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        self.ansi_processor.advance(&mut self.term, bytes);
        let needs_redraw = self.ansi_processor.sync_bytes_count() < bytes.len();
        if needs_redraw {
            self.content_dirty = true;
        }

        FeedOutputResult { needs_redraw, sync_pending: self.has_pending_sync_update() }
    }

    /// Flush a synchronized update after its timeout elapses.
    ///
    /// Returns `true` when buffered synchronized bytes were committed to the
    /// terminal and the pane should be redrawn.
    pub fn flush_sync_timeout(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.sync_deadline() else { return false };
        if deadline > now {
            return false;
        }

        self.ansi_processor.stop_sync(&mut self.term);
        self.content_dirty = true;
        true
    }

    /// Deadline for the current synchronized update, if one is pending.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.ansi_processor.sync_timeout().sync_timeout()
    }

    /// Whether a synchronized update is currently buffering terminal output.
    pub fn has_pending_sync_update(&self) -> bool {
        self.sync_deadline().is_some()
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
        self.content_dirty = true;
    }

    /// Resize this pane to a new pixel rect.
    ///
    /// Returns the new grid dimensions (cols, rows) for sending to the server.
    pub fn resize(&mut self, new_rect: Rect, new_grid: GridSize) -> GridSize {
        let old_cols = self.grid.cols;
        self.rect = new_rect;
        self.grid = new_grid;
        self.content_dirty = true;

        let dims = TermDims { cols: usize::from(new_grid.cols), lines: usize::from(new_grid.rows) };
        if self.term.columns() != dims.cols || self.term.screen_lines() != dims.lines {
            if new_grid.cols < old_cols.saturating_sub(5) || (old_cols > 1 && new_grid.cols <= 30) {
                tracing::warn!(
                    session_id = %self.session_id,
                    old_cols,
                    new_cols = new_grid.cols,
                    new_rows = new_grid.rows,
                    rect_w = new_rect.width,
                    rect_h = new_rect.height,
                    "pane columns shrank significantly"
                );
            }
            self.term.resize(dims);
        }

        new_grid
    }

    /// Return `true` when the running application has requested mouse events.
    pub fn has_mouse_mode(&self) -> bool {
        self.term.mode().contains(alacritty_terminal::term::TermMode::MOUSE_MODE)
    }

    /// Return the pixel offset where terminal content starts (below tab bar and prompt bar).
    pub fn content_offset(
        &self,
        tab_bar_height: f32,
        prompt_bar_height: f32,
        padding: &ContentPadding,
    ) -> (f32, f32) {
        let eff = effective_padding(padding, self.edges);
        let tbh = if self.edges.top { tab_bar_height } else { 0.0 };
        (self.rect.x + eff.left, self.rect.y + tbh + prompt_bar_height + eff.top)
    }

    /// Return the content area (below tab bar) as a viewport size tuple.
    #[allow(dead_code, reason = "public API for pane viewport queries")]
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "pane rect dimensions are small non-negative pixel values that fit in u32"
    )]
    pub fn content_viewport(&self, tab_bar_height: f32) -> (u32, u32) {
        let tbh = if self.edges.top { tab_bar_height } else { 0.0 };
        let h = (self.rect.height - tbh).max(1.0);
        (self.rect.width.max(1.0) as u32, h as u32)
    }

    /// Prefer a Codex task label over the terminal title when one is active.
    pub fn preferred_tab_title(&self) -> &str {
        self.codex_task_label.as_deref().unwrap_or(&self.title)
    }

    /// Pixel height of the prompt bar for this pane.
    ///
    /// Returns 0.0 when `prompt_bar` is disabled or no prompts have been received.
    /// One prompt = 1 line + padding. Two or more = 2 lines + padding.
    pub fn prompt_bar_height(&self, cell_height: f32, prompt_bar_enabled: bool) -> f32 {
        if !prompt_bar_enabled || self.prompt_bar_dismissed || self.prompt_count == 0 {
            return 0.0;
        }
        let lines = if self.prompt_count == 1 { 1.0 } else { 2.0 };
        lines * cell_height + 8.0 + 8.0 // top_pad + bottom_pad
    }
}

/// Compute effective content padding for a pane, zeroing out padding on
/// internal edges (those adjacent to a sibling pane).
pub fn effective_padding(padding: &ContentPadding, edges: PaneEdges) -> ContentPadding {
    ContentPadding {
        top: if edges.top { padding.top } else { 0.0 },
        right: if edges.right { padding.right } else { 0.0 },
        bottom: if edges.bottom { padding.bottom } else { 0.0 },
        left: if edges.left { padding.left } else { 0.0 },
    }
}

/// Compute the grid size for a pane's content area.
#[allow(
    clippy::too_many_arguments,
    reason = "layout requires rect, two cell dims, two bar heights, and padding"
)]
pub fn compute_pane_grid(
    rect: Rect,
    cell_width: f32,
    cell_height: f32,
    tab_bar_height: f32,
    prompt_bar_height: f32,
    padding: &ContentPadding,
) -> GridSize {
    let content_w = (rect.width - padding.left - padding.right).max(1.0);
    let content_h =
        (rect.height - tab_bar_height - prompt_bar_height - padding.top - padding.bottom).max(1.0);
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
