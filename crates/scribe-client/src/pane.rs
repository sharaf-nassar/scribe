//! Per-pane state: terminal emulator and ANSI processor.
//!
//! Each pane owns a [`Term`] and a VTE [`Processor`]. Rendering is
//! performed by the shared [`TerminalRenderer`] in `GpuContext`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use scribe_common::config::ContentPadding;
use scribe_common::ids::{SessionId, WorkspaceId};
use scribe_common::protocol::SessionContext;
use scribe_pty::sync_update_filter::SyncUpdateFrameSplitter;
use scribe_renderer::types::{CellInstance, GridSize};

use crate::layout::{PaneEdges, Rect};
use crate::restore_state::LaunchBinding;
use crate::scrollbar::ScrollbarState;
use crate::selection::SelectionRange;
use crate::split_scroll::SplitScrollState;

/// Mirror vte's synchronized-update timeout for raw frames still buffered
/// ahead of the pane-local ANSI processor.
const RAW_SYNC_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, Default)]
pub struct PromptUiState {
    /// Whether the current prompt has `click_events=1` enabled (OSC 133;A).
    pub click_events: bool,
    /// Whether the user has dismissed the prompt bar for this pane.
    pub dismissed: bool,
}

/// State for a single terminal pane.
pub struct Pane {
    pub session_id: SessionId,
    pub launch_binding: LaunchBinding,
    pub workspace_id: WorkspaceId,
    pub workspace_name: Option<String>,
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
    /// Absolute position where the prompt input starts (OSC 133;B mark).
    /// `Some((absolute_line, column))` while waiting for user input.
    /// Cleared when a command starts or ends (OSC 133;C / D).
    pub input_start: Option<(usize, usize)>,
    /// PTY output chunks queued behind the current frame so light bursts can
    /// animate incrementally while larger backlogs can be coalesced before
    /// the next redraw.
    pub pending_output_frames: VecDeque<Vec<u8>>,
    /// Streaming raw-frame splitter that preserves `CSI ? 2026 h/l`
    /// boundaries across arbitrary PTY IPC chunking.
    sync_output_frames: SyncUpdateFrameSplitter,
    /// Timeout for raw synchronized-update bytes that have not reached the
    /// pane-local ANSI processor yet.
    sync_output_deadline: Option<Instant>,
    /// Text of the first user prompt submitted in this AI session.
    pub first_prompt: Option<String>,
    /// Text of the most recent user prompt (differs from first after 2+ prompts).
    pub latest_prompt: Option<String>,
    /// Total number of prompts received in this session.
    pub prompt_count: u32,
    /// Last-seen `conversation_id` for detecting session resets.
    pub last_conversation_id: Option<String>,
    /// Prompt-bar interaction flags that affect hit testing and visibility.
    pub prompt_ui: PromptUiState,
    /// Split-scroll state: `Some` when the pane is scrolled up with the
    /// live-bottom pin active (AI panes with `scroll_pin` enabled).
    pub split_scroll: Option<SplitScrollState>,
}

#[derive(Clone, Copy)]
pub struct PaneLayoutState {
    pub rect: Rect,
    pub grid: GridSize,
    pub edges: PaneEdges,
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
    pub fn new(
        layout: PaneLayoutState,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        launch_binding: LaunchBinding,
    ) -> Self {
        let PaneLayoutState { rect, grid, edges } = layout;
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
            input_start: None,
            pending_output_frames: VecDeque::new(),
            sync_output_frames: SyncUpdateFrameSplitter::new(),
            sync_output_deadline: None,
            first_prompt: None,
            latest_prompt: None,
            prompt_count: 0,
            last_conversation_id: None,
            prompt_ui: PromptUiState::default(),
            split_scroll: None,
        }
    }

    /// Queue raw PTY output frames, preserving synchronized-update commit
    /// boundaries across IPC message splits.
    pub fn queue_output_frames(&mut self, bytes: &[u8]) -> bool {
        let frames = self.sync_output_frames.split_frames(bytes);
        self.sync_output_deadline = self.sync_output_frames.inside_sync().then(|| {
            self.sync_output_deadline.unwrap_or_else(|| Instant::now() + RAW_SYNC_TIMEOUT)
        });
        if frames.is_empty() {
            return false;
        }

        self.pending_output_frames.extend(frames);
        true
    }

    /// Drop any staged PTY frames and reset the sync-frame splitter.
    pub fn reset_output_queue(&mut self) {
        self.pending_output_frames.clear();
        self.sync_output_frames = SyncUpdateFrameSplitter::new();
        self.sync_output_deadline = None;
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
        let raw_timed_out = self.sync_output_deadline.is_some_and(|deadline| deadline <= now);
        let mut flushed_any = false;

        if raw_timed_out {
            self.sync_output_deadline = None;
            if let Some(bytes) = self.sync_output_frames.flush_timed_out() {
                self.feed_output(&bytes);
                flushed_any = true;
            }
        }

        let parser_timed_out = self
            .ansi_processor
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= now);
        if parser_timed_out {
            self.ansi_processor.stop_sync(&mut self.term);
            self.content_dirty = true;
            flushed_any = true;
        }

        flushed_any
    }

    /// Deadline for the current synchronized update, if one is pending.
    pub fn sync_deadline(&self) -> Option<Instant> {
        match (self.sync_output_deadline, self.ansi_processor.sync_timeout().sync_timeout()) {
            (Some(raw), Some(parser)) => Some(raw.min(parser)),
            (Some(raw), None) => Some(raw),
            (None, Some(parser)) => Some(parser),
            (None, None) => None,
        }
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
        scale_factor: f32,
    ) -> (f32, f32) {
        let eff = effective_padding(padding, self.edges, scale_factor);
        let tbh = if self.edges.top() { tab_bar_height } else { 0.0 };
        (self.rect.x + eff.left, self.rect.y + tbh + prompt_bar_height + eff.top)
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
        if !prompt_bar_enabled || self.prompt_ui.dismissed || self.prompt_count == 0 {
            return 0.0;
        }
        crate::prompt_bar::prompt_bar_height(self.prompt_count, cell_height)
    }
}

/// Compute effective content padding for a pane, zeroing out padding on
/// internal edges (those adjacent to a sibling pane) and scaling by the
/// display scale factor for physical-pixel rendering.
pub fn effective_padding(
    padding: &ContentPadding,
    edges: PaneEdges,
    scale_factor: f32,
) -> ContentPadding {
    ContentPadding {
        top: if edges.top() { padding.top * scale_factor } else { 0.0 },
        right: if edges.right() { padding.right * scale_factor } else { 0.0 },
        bottom: if edges.bottom() { padding.bottom * scale_factor } else { 0.0 },
        left: if edges.left() { padding.left * scale_factor } else { 0.0 },
    }
}

/// Compute the grid size for a pane's content area.
pub struct PaneGridRequest<'a> {
    pub rect: Rect,
    pub cell_size: (f32, f32),
    pub tab_bar_height: f32,
    pub prompt_bar_height: f32,
    pub padding: &'a ContentPadding,
}

pub fn compute_pane_grid(request: &PaneGridRequest<'_>) -> GridSize {
    let rect = request.rect;
    let (cell_width, cell_height) = request.cell_size;
    let padding = request.padding;
    let content_w = (rect.width - padding.left - padding.right).max(1.0);
    let content_h = (rect.height
        - request.tab_bar_height
        - request.prompt_bar_height
        - padding.top
        - padding.bottom)
        .max(1.0);
    grid_from_pixels(content_w, content_h, cell_width, cell_height)
}

fn grid_axis_units(extent: f32, cell_size: f32) -> u16 {
    if cell_size <= 0.0 || !extent.is_finite() || extent <= 0.0 {
        return 1;
    }

    let mut low = 0u16;
    let mut high = u16::MAX;
    while low < high {
        let mid = low + (high - low).saturating_add(1) / 2;
        if f32::from(mid) * cell_size <= extent {
            low = mid;
        } else {
            high = mid.saturating_sub(1);
        }
    }

    low.max(1)
}

/// Compute grid dimensions from pixel dimensions and cell size.
fn grid_from_pixels(width: f32, height: f32, cell_w: f32, cell_h: f32) -> GridSize {
    let cols = grid_axis_units(width, cell_w);
    let rows = grid_axis_units(height, cell_h);
    GridSize { cols, rows }
}
