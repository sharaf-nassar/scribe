//! Display-only terminal state adapted from Zed's terminal model.
//!
//! The state owns no PTY. It receives bytes from Scribe IPC, advances Zed's
//! Alacritty fork, and exposes a render-ready grid snapshot to GPUI.
//!
//! It is also the shell's single seam onto the ported terminal-navigation
//! modules, because they all need the live `Term` this type owns: the viewport
//! is scrolled here ([`DisplayOnlyTerminal::scroll`]), vi / copy mode is
//! toggled and driven here through [`scribe_client::vi_mode`], the
//! split-scroll pin is folded into the snapshot through
//! [`scribe_client::split_scroll`], a click resolves its
//! [`scribe_client::smart_selection`] candidates here, and an OSC 133 mark
//! reads its anchor row — and a mark-relative jump lands on one — through the
//! absolute-row helpers this type exposes.
//!
//! A window shows one grid per pane, so [`PaneGrids`] keys a [`PaneGrid`] per
//! session: the coalescing drain already carries a `SessionId` with every
//! batch, and the paint path asks for the pane belonging to the session each
//! pane is showing.
//!
//! Each [`PaneGrid`] is split in two halves with opposite locking needs.
//! [`PaneStream`] — the sync-frame queue and the grid it feeds — is held for as
//! long as a VTE parse takes, which under a firehose is longer than a frame.
//! [`PaneFrame`] is the projection the renderer reads, republished out of the
//! stream after every change, so a paint never queues behind a parse and one
//! pane's firehose never stalls another's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use scribe_common::ids::SessionId;
use scribe_common::terminal_images::{
    TerminalGridEffect, TerminalImageLiveMessage, TerminalImageReplayMessage,
};

pub use alacritty_terminal_gpui::grid::Scroll;
pub use alacritty_terminal_gpui::term::cell::Flags;
use alacritty_terminal_gpui::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line},
    term::{Config, Osc52, Term, TermMode},
};
use scribe_client::mouse_reporting::{MotionReporting, MouseModes, MouseReportMode};
use scribe_client::scrollbar::ScrollMetrics;
use scribe_client::selection::{
    SelectionMode, SelectionPoint, SelectionSpan, SelectionState, viewport_spans,
};
use scribe_client::smart_selection::{CompiledSmartSelection, SmartSelectionCandidate};
use scribe_client::split_scroll::{
    SplitScrollEligibility, align_pin_rows_to_logical_lines, compute_pin_rows,
    split_scroll_eligible,
};
use scribe_client::terminal_image_scene::{
    CommittedImageScene, LiveImageScene, LiveSceneApply, LiveSceneError,
    filter_terminal_image_placeholders,
};
use scribe_client::url_detect::{PaneUrlCache, SpanKind};
use scribe_client::vi_mode::{self, ViMotion};
use vte::ansi::{Color, CursorShape as TerminalCursorShape, Handler as _};

use crate::session_lifecycle::PromptAnchor;
use crate::sync_frames::{
    FeedOutputResult, OutputTarget, SyncFrameQueue, flush_before_rebuild,
    present_next_burst as present_queued_burst,
};

/// One rendered terminal cell: its character plus the raw SGR state the paint
/// path needs to colour and decorate it.
///
/// The colour fields stay in alacritty's own `Color` space rather than being
/// resolved here, so a live theme edit repaints existing content without
/// re-running the parser: [`crate::terminal_element::TerminalElement`] resolves
/// them against the current theme on every frame.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character in this cell (a blank cell holds a space).
    pub c: char,
    /// Raw foreground colour before bold-bright / INVERSE / DIM are applied.
    pub fg: Color,
    /// Raw background colour before INVERSE is applied.
    pub bg: Color,
    /// SGR attributes: BOLD, ITALIC, UNDERLINE, STRIKEOUT, INVERSE, DIM,
    /// HIDDEN, and the wide-char bookkeeping flags.
    pub flags: Flags,
    /// Up to three Kitty placeholder coordinate marks retained from Alacritty.
    pub zerowidth: [char; 3],
    /// Number of initialized entries in [`Self::zerowidth`].
    pub zerowidth_len: u8,
    /// Raw underline colour, which Kitty placeholders use as placement ID.
    pub underline_color: Option<Color>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Named(vte::ansi::NamedColor::Foreground),
            bg: Color::Named(vte::ansi::NamedColor::Background),
            flags: Flags::empty(),
            zerowidth: ['\0'; 3],
            zerowidth_len: 0,
            underline_color: None,
        }
    }
}

impl Cell {
    /// Combining marks retained for Kitty placeholder resolution.
    #[must_use]
    pub fn zerowidth(&self) -> &[char] {
        self.zerowidth.get(..usize::from(self.zerowidth_len)).unwrap_or(&self.zerowidth)
    }
}

/// A cell address inside a [`Content`] snapshot, in viewport coordinates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewportPoint {
    /// Row index into [`Content::rows`].
    pub row: usize,
    /// Column index into the row.
    pub col: usize,
}

/// Shape requested by the terminal application for the live shell cursor.
///
/// `Block` remains the default-config sentinel, matching the legacy renderer:
/// an explicit beam or underline from DECSCUSR wins, while a block uses the
/// user's `appearance.cursor_shape`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellCursorShape {
    /// Use the configured block/beam/underline fallback.
    Block,
    /// Draw a vertical beam.
    Beam,
    /// Draw a rule at the bottom of the cell.
    Underline,
}

/// Shell cursor projected into the immutable viewport snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShellCursor {
    /// Cursor cell in viewport coordinates.
    pub point: ViewportPoint,
    /// Shape selected by the terminal application.
    pub shape: ShellCursorShape,
}

/// Where a pane's shell cursor sits, plus the viewport placement needed to
/// resolve an absolute scrollback row back onto the painted grid.
///
/// Read once per frame by the IME path: `abs_row`/`col` become the composition
/// anchor and the OS candidate window's spot, and the remaining fields are the
/// live half of
/// [`PreeditGeometry`](scribe_client::preedit::PreeditGeometry) the paint
/// pass completes with its own pixel metrics.
#[derive(Clone, Copy, Debug)]
pub struct CursorPlacement {
    /// The cursor's row counted from the oldest surviving scrollback line.
    pub abs_row: usize,
    /// The cursor's column.
    pub col: usize,
    /// Grid width in columns.
    pub columns: u16,
    /// Rows in the live screen.
    pub screen_lines: usize,
    /// Rows the viewport is scrolled up into scrollback (0 at the bottom).
    pub display_offset: usize,
    /// Absolute scrollback row of the topmost visible line.
    pub viewport_top_abs_row: usize,
}

/// Immutable grid snapshot consumed by [`crate::terminal_element::TerminalElement`].
#[derive(Clone, Default)]
pub struct Content {
    /// Visible rows, including blank cells so every row keeps terminal width.
    pub rows: Vec<Vec<Cell>>,
    /// Rows the pane is scrolled above its live bottom.
    pub display_offset: usize,
    /// How many trailing rows of [`Self::rows`] show the *live* screen while
    /// the rows above them show scrollback — the split-scroll pin. `0` whenever
    /// split-scroll is inactive, which is the ordinary case.
    pub pin_rows: usize,
    /// Where the vi / copy-mode cursor sits in this snapshot, or `None` when vi
    /// mode is off or the cursor scrolled out of the painted viewport.
    pub vi_cursor: Option<ViewportPoint>,
    /// Focus-independent shell cursor state projected into the painted
    /// viewport. `None` while DECTCEM hides it, vi mode owns the keyboard
    /// cursor, or ordinary scrollback has moved the live cursor off-screen.
    pub shell_cursor: Option<ShellCursor>,
}

/// Plain-text views of a snapshot. The paint path consumes cells directly, so
/// these exist for assertions that only care about what the grid says.
#[cfg(test)]
impl Content {
    /// The plain text of one visible row.
    pub fn row_text(&self, row: usize) -> String {
        self.rows
            .get(row)
            .map(|cells| cells.iter().map(|cell| cell.c).collect())
            .unwrap_or_default()
    }

    /// The whole viewport as newline-joined plain text.
    pub fn text(&self) -> String {
        (0..self.rows.len()).map(|row| self.row_text(row)).collect::<Vec<_>>().join("\n")
    }
}

#[derive(Clone, Copy)]
struct TerminalDimensions {
    columns: usize,
    lines: usize,
}

impl alacritty_terminal_gpui::grid::Dimensions for TerminalDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Alacritty terminal with Zed's display-only ownership model.
/// A link the pointer is over: what activating it opens, and where to draw it.
///
/// One value serves both consumers so the underline can never cover a
/// different span than the one a click would follow.
pub struct HoveredLink {
    /// Whether this is an explicit OSC 8 hyperlink, a heuristic URL, or a path.
    /// Each opens through a different route: OSC 8 through the scheme-allowlist
    /// gate, a URL through the allowlist directly, a path through the OS
    /// handler with the pane's CWD.
    pub kind: SpanKind,
    /// The URI or path text, verbatim.
    pub target: String,
    /// The viewport rows the span covers, one span per row. A wrapped or
    /// hard-break-joined link covers several, and a continuation row starts at
    /// its indent rather than column 0.
    pub rows: Vec<SelectionSpan>,
}

pub struct DisplayOnlyTerminal {
    term: Term<VoidListener>,
    output_processor: vte::ansi::Processor,
    /// Shared rather than owned so republishing the render projection after a
    /// parse is a refcount bump instead of a copy of every painted row.
    content: Arc<Content>,
    /// Whether an advance changed grid state [`Self::content`] has not been
    /// rebuilt for yet. Set by [`Self::advance_output`] and cleared by every
    /// rebuild, so the pacer can drain through a backlog and pay for one
    /// snapshot instead of one per frame.
    content_stale: bool,
    /// Config + AI-provider gate for split-scroll, pushed in by the shell on
    /// every frame. The live half of the decision (scrolled up, normal screen)
    /// is read off the terminal itself in [`Self::active_pin_rows`].
    split_scroll: SplitScrollEligibility,
    /// Live mouse-drag selection over this pane's grid. Ranges are absolute
    /// grid lines, so scrolling moves the highlight with the content rather
    /// than leaving it pinned to the screen.
    selection: SelectionState,
    /// Atomically published CPU-only terminal-image state. GPU resources stay
    /// outside this model and land with the renderer bead.
    image_scene: LiveImageScene,
    /// Detected URL / path / OSC 8 spans over the visible grid, rescanned
    /// lazily. Invalidated by [`Self::make_content`], which is the one place
    /// every path that can move a visible cell already funnels through.
    urls: PaneUrlCache,
    /// Scrollback capacity this grid was built with, restored after a
    /// [`Self::trim_history`] shrinks the ring to drop rows.
    scrollback_lines: usize,
}

impl DisplayOnlyTerminal {
    /// Creates an empty terminal at the dimensions sent with `AttachSessions`.
    pub fn new(columns: usize, lines: usize) -> Self {
        let dimensions = TerminalDimensions { columns, lines };
        let config = Config { kitty_keyboard: true, osc52: Osc52::Disabled, ..Config::default() };
        let scrollback_lines = config.scrolling_history;
        let term = Term::new(config, &dimensions, VoidListener);
        let mut terminal = Self {
            term,
            output_processor: vte::ansi::Processor::new(),
            content: Arc::default(),
            content_stale: false,
            scrollback_lines,
            split_scroll: SplitScrollEligibility::default(),
            selection: SelectionState::new(),
            image_scene: LiveImageScene::default(),
            urls: PaneUrlCache::new(),
        };
        terminal.make_content();
        terminal
    }

    /// Advances one committed frame and reports whether it changed visible
    /// state and whether a synchronized update is still buffering in the
    /// parser. Bytes wholly absorbed by an open synchronized update change
    /// nothing visible, mirroring the winit client's `Pane::feed_output`.
    ///
    /// The content snapshot is deliberately *not* rebuilt here. The pacer
    /// presents one committed burst per redraw and drains through everything
    /// behind it, so every frame but the last of a pass is parsed and then
    /// overwritten before anything paints; rebuilding each one would spend the
    /// pane lock on screens no one ever sees. [`Self::publish_content`] does it
    /// once, for the state the burst actually leaves behind.
    pub fn advance_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        self.output_processor.advance(&mut self.term, bytes);
        let needs_redraw = self.output_processor.sync_bytes_count() < bytes.len();
        self.content_stale |= needs_redraw;
        FeedOutputResult { needs_redraw, sync_pending: self.parser_sync_deadline().is_some() }
    }

    /// Rebuilds the content snapshot when advances left it stale, reporting
    /// whether it rebuilt.
    ///
    /// Every completed presentation calls this before it returns, so a snapshot
    /// read off the pane is never behind the grid: what the pacer skips is the
    /// rebuild per intermediate frame, never the rebuild itself.
    pub fn publish_content(&mut self) -> bool {
        if !self.content_stale {
            return false;
        }
        self.make_content();
        true
    }

    /// Replace the terminal from authoritative ANSI and publish the final
    /// viewport exactly once.
    ///
    /// The pane queue must be flushed first. That makes the viewport captured
    /// here authoritative when queued output includes a real ED 3. A scrolled
    /// anchor is shifted by any oldest rows the replacement dropped, while a
    /// live-bottom viewport stays at the live bottom.
    fn rebuild(&mut self, bytes: &[u8], columns: usize, lines: usize, kept_rows: usize) -> bool {
        let old_history = self.history_size();
        let viewport_top = (self.display_offset() > 0).then(|| self.viewport_top_abs());
        let reshaped = columns > 0 && lines > 0 && self.dimensions() != (columns, lines);
        if reshaped {
            self.term.resize(TerminalDimensions { columns, lines });
        }
        let rebuilt = self.advance_output(bytes).needs_redraw;
        let trimmed = self.trim_history_without_publish(kept_rows) > 0;
        let new_history = self.history_size();
        let restored = viewport_top.is_some_and(|old_top| {
            let dropped = old_history.saturating_sub(new_history);
            self.scroll_to_abs_without_publish(old_top.saturating_sub(dropped).min(new_history))
        });
        self.content_stale |= reshaped || trimmed || restored;
        self.publish_content();
        rebuilt || reshaped || trimmed || restored
    }

    /// Advances one committed frame and publishes the snapshot it produced.
    ///
    /// The unpaced pairing of [`Self::advance_output`] and
    /// [`Self::publish_content`], for tests that drive the grid a frame at a
    /// time and read it back; production output goes through the drain, which
    /// publishes once per burst instead.
    #[cfg(test)]
    pub fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        let result = self.advance_output(bytes);
        self.publish_content();
        result
    }

    /// Deadline of the parser-side synchronized update, if one is open.
    #[must_use]
    pub fn parser_sync_deadline(&self) -> Option<Instant> {
        self.output_processor.sync_timeout().sync_timeout()
    }

    /// Commits an expired parser-side synchronized update at `now`, refreshing
    /// the content snapshot. Returns `true` when an update was flushed.
    pub fn flush_parser_sync_timeout(&mut self, now: Instant) -> bool {
        if self.parser_sync_deadline().is_some_and(|deadline| deadline <= now) {
            self.output_processor.stop_sync(&mut self.term);
            self.make_content();
            true
        } else {
            false
        }
    }

    /// Returns the content captured after the most recent output frame.
    pub fn content(&self) -> Arc<Content> {
        Arc::clone(&self.content)
    }

    /// Apply one ordered live image record beside the text parser.
    pub fn apply_image_live(
        &mut self,
        message: TerminalImageLiveMessage,
    ) -> Result<bool, LiveSceneError> {
        let outcome = self.image_scene.apply(message)?;
        let LiveSceneApply::Committed(scene) = outcome else {
            return Ok(false);
        };
        for effect in &scene.last_grid_effects {
            self.apply_image_grid_effect(effect);
        }
        self.make_content();
        Ok(true)
    }

    /// Apply one generation-tagged image replay record beside the text parser.
    ///
    /// The snapshot stages off-screen; only its commit swaps the pane's scene,
    /// and that commit also drains the live records that arrived behind it.
    pub fn apply_image_replay(
        &mut self,
        message: TerminalImageReplayMessage,
    ) -> Result<bool, LiveSceneError> {
        let outcome = self.image_scene.apply_replay(message)?;
        let LiveSceneApply::Committed(scene) = outcome else {
            return Ok(false);
        };
        for effect in &scene.last_grid_effects {
            self.apply_image_grid_effect(effect);
        }
        self.make_content();
        Ok(true)
    }

    /// Current immutable CPU image scene for future paint/cache consumers.
    #[must_use]
    pub fn image_scene(&self) -> Arc<CommittedImageScene> {
        self.image_scene.committed()
    }

    fn apply_image_grid_effect(&mut self, effect: &TerminalGridEffect) {
        match *effect {
            TerminalGridEffect::MoveCursor { row, column } => {
                self.term.goto(row, usize::from(column));
            }
            TerminalGridEffect::Scroll { top, bottom, rows } if rows != 0 => {
                let lines = self.term.screen_lines();
                let start = usize::from(top).min(lines);
                let end = usize::from(bottom).saturating_add(1).min(lines);
                if start >= end {
                    return;
                }
                let region = Line(i32::try_from(start).unwrap_or(i32::MAX))
                    ..Line(i32::try_from(end).unwrap_or(i32::MAX));
                let positions =
                    usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX).min(end - start);
                if rows > 0 {
                    self.term.grid_mut().scroll_up::<Color>(&region, positions);
                } else {
                    self.term.grid_mut().scroll_down(&region, positions);
                }
            }
            TerminalGridEffect::Scroll { .. }
            | TerminalGridEffect::EraseCells { .. }
            | TerminalGridEffect::ResizeClip { .. }
            | TerminalGridEffect::SwitchScreen { .. }
            | TerminalGridEffect::SoftReset
            | TerminalGridEffect::HardReset => {}
        }
    }

    /// Current viewport geometry in terminal cells.
    ///
    /// The IPC reconnect path uses this to reattach every visible pane at the
    /// size it had before a server handoff, instead of temporarily expanding
    /// split panes to the startup window dimensions.
    #[must_use]
    pub fn dimensions(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// Reshape the display grid to `columns` x `lines`.
    ///
    /// A pane split changes how much of the window a session owns, so the
    /// display grid has to follow the `Resize` the client sends the server:
    /// alacritty reflows its own rows here, and the authoritative repaint
    /// arrives right after as the `ScreenSnapshot` the caller requests. The
    /// content snapshot is rebuilt immediately so the very next frame paints at
    /// the new geometry instead of a grid the pane can no longer hold.
    pub fn resize(&mut self, columns: usize, lines: usize) {
        if columns == 0 || lines == 0 {
            return;
        }
        self.term.resize(TerminalDimensions { columns, lines });
        self.make_content();
    }

    #[cfg(test)]
    fn visible_text(&self) -> String {
        self.content.text()
    }

    /// Move the display viewport and rebuild the snapshot when it moved.
    ///
    /// Returns `true` when the display offset actually changed, so the caller
    /// can skip the repaint and the scroll-derived bookkeeping for a scroll
    /// that hit the end of the scrollback.
    pub fn scroll(&mut self, scroll: Scroll) -> bool {
        let changed = self.scroll_without_publish(scroll);
        if changed {
            self.make_content();
        }
        changed
    }

    fn scroll_without_publish(&mut self, scroll: Scroll) -> bool {
        let before = self.term.grid().display_offset();
        self.term.scroll_display(scroll);
        self.term.grid().display_offset() != before
    }

    /// How far the viewport is scrolled into the scrollback, in rows.
    #[must_use]
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Rows of committed scrollback above the live screen.
    #[must_use]
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Drop the oldest scrollback rows until only `kept_rows` remain, returning
    /// how many rows actually went.
    ///
    /// This replicates the trim the server performs on its own `Term` when it
    /// suppresses an AI session's ED 3: the sequence never reaches the client
    /// (it was filtered out of the byte stream), so without this the display
    /// grid would keep rows the server has already forgotten and every
    /// absolute-row anchor — prompt-jump targets and scrollbar command ticks —
    /// would drift a little further from the server's on every redraw.
    ///
    /// Shrinking and re-growing the ring is how alacritty exposes the drop, and
    /// it is the same two-step the server uses, so both ends land on the same
    /// surviving rows.
    pub fn trim_history(&mut self, kept_rows: usize) -> usize {
        let dropped = self.trim_history_without_publish(kept_rows);
        if dropped > 0 {
            self.make_content();
        }
        dropped
    }

    fn trim_history_without_publish(&mut self, kept_rows: usize) -> usize {
        let before = self.history_size();
        if kept_rows >= before {
            return 0;
        }
        let max_rows = self.scrollback_lines;
        let grid = self.term.grid_mut();
        grid.update_history(kept_rows.min(max_rows));
        grid.update_history(max_rows);
        before.saturating_sub(self.history_size())
    }

    /// The viewport measurements the overlay scrollbar sizes its thumb from.
    ///
    /// Read on the paint pass rather than cached on [`Content`]: the snapshot
    /// is rebuilt only when visible cells change, while the thumb has to track
    /// a scrollback that grows on every committed row.
    #[must_use]
    pub fn scroll_metrics(&self) -> ScrollMetrics {
        ScrollMetrics {
            history_size: self.history_size(),
            screen_lines: self.term.screen_lines(),
            display_offset: self.display_offset(),
        }
    }

    /// The absolute scrollback row the top of the viewport is showing.
    ///
    /// Absolute rows count from the oldest surviving scrollback line, which is
    /// the space [`crate::session_lifecycle::PromptMarks`] anchors marks in, so
    /// a jump is a comparison in one coordinate system rather than a conversion.
    #[must_use]
    pub fn viewport_top_abs(&self) -> usize {
        self.history_size().saturating_sub(self.display_offset())
    }

    /// The grid geometry a prompt mark anchors against right now.
    ///
    /// Read after the mark's preceding output has been applied, so the cursor
    /// row is the row the shell drew the prompt on.
    #[must_use]
    pub fn prompt_anchor(&self) -> PromptAnchor {
        PromptAnchor {
            history: self.history_size(),
            screen_lines: self.term.screen_lines(),
            cursor_row: self.cursor_line(),
            cursor_col: self.term.grid().cursor.point.column.0,
        }
    }

    /// Where this pane's shell cursor sits and how its viewport is placed, in
    /// the absolute scrollback coordinates an IME composition anchors in.
    ///
    /// The preedit overlay is pinned to the line composition started on, so it
    /// needs the cursor in the same absolute space [`Self::viewport_top_abs`]
    /// reports; the remaining fields are what
    /// [`crate::preedit::compute_overlay`](scribe_client::preedit::compute_overlay)
    /// resolves that anchor back onto the visible grid with.
    #[must_use]
    pub fn cursor_placement(&self) -> CursorPlacement {
        let anchor = self.prompt_anchor();
        CursorPlacement {
            abs_row: anchor.history.saturating_add(anchor.cursor_row),
            col: anchor.cursor_col,
            columns: u16::try_from(self.term.columns()).unwrap_or(u16::MAX),
            screen_lines: anchor.screen_lines,
            display_offset: self.display_offset(),
            viewport_top_abs_row: self.viewport_top_abs(),
        }
    }

    /// Scroll the viewport so `abs_pos` becomes its top row.
    ///
    /// Returns `true` when the viewport actually moved. Both prompt jumps and
    /// the failure jump land here, so they cannot drift from each other or from
    /// [`Self::scroll`]'s snapshot bookkeeping.
    pub fn scroll_to_abs(&mut self, abs_pos: usize) -> bool {
        let changed = self.scroll_to_abs_without_publish(abs_pos);
        if changed {
            self.make_content();
        }
        changed
    }

    fn scroll_to_abs_without_publish(&mut self, abs_pos: usize) -> bool {
        let offset = self.history_size().saturating_sub(abs_pos);
        let delta = grid_i32(offset).saturating_sub(grid_i32(self.display_offset()));
        delta != 0 && self.scroll_without_publish(Scroll::Delta(delta))
    }

    /// Scroll the viewport to an absolute `display_offset`.
    ///
    /// The scrollbar's click-to-jump and thumb drag both compute a target
    /// offset directly (the track *is* the scrollback, so a Y position maps to
    /// an offset), and routing them through here keeps them on the same
    /// snapshot bookkeeping as [`Self::scroll`]. Returns `true` when the
    /// viewport actually moved.
    pub fn scroll_to_offset(&mut self, offset: usize) -> bool {
        let delta = grid_i32(offset).saturating_sub(grid_i32(self.display_offset()));
        if delta == 0 {
            return false;
        }
        self.scroll(Scroll::Delta(delta))
    }

    /// Push the config + AI-provider half of the split-scroll decision in.
    ///
    /// Returns `true` when the snapshot changed as a result, which happens both
    /// when the gate opens (the pin appears) and when it closes (the viewport
    /// goes back to a single scrolled region).
    pub fn set_split_scroll_eligibility(&mut self, eligibility: SplitScrollEligibility) -> bool {
        if self.split_scroll == eligibility {
            return false;
        }
        let before = self.content.pin_rows;
        self.split_scroll = eligibility;
        self.make_content();
        self.content.pin_rows != before
    }

    /// How many rows the split-scroll pin currently occupies (`0` when off).
    #[must_use]
    pub fn pin_rows(&self) -> usize {
        self.content.pin_rows
    }

    /// Toggle vi / copy mode and refresh the snapshot's vi cursor.
    pub fn toggle_vi_mode(&mut self) {
        vi_mode::toggle_vi_mode(&mut self.term);
        self.make_content();
    }

    /// Drive the vi cursor with `motion`, refreshing the snapshot.
    ///
    /// The fork scrolls the display to keep the vi cursor visible, so a motion
    /// that walks off the top of the viewport moves the viewport with it.
    pub fn vi_motion(&mut self, motion: ViMotion) {
        vi_mode::vi_motion(&mut self.term, motion);
        self.make_content();
    }

    /// Whether vi / copy mode is currently active.
    #[must_use]
    pub fn is_vi_mode(&self) -> bool {
        vi_mode::is_vi_mode(&self.term)
    }

    /// Whether the pane's application has enabled bracketed paste (DEC 2004).
    ///
    /// A paste is wrapped in the DEC 2004 markers only when this is set, and
    /// the spec-011 confirmation gate defers to the application entirely when
    /// it is: a program that opted into bracketed paste is already able to tell
    /// pasted bytes from typed ones.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The mouse-related DEC private modes this pane's application has enabled.
    ///
    /// `MOUSE_MODE` is a *union* of three bits (1000 / 1002 / 1003) that
    /// alacritty stores mutually exclusively — each DECSET clears the union and
    /// sets exactly one bit — so the tracking test has to be `intersects`;
    /// `contains` would require all three and never match. Every other flag
    /// here is a single bit and keeps `contains`.
    #[must_use]
    pub fn mouse_modes(&self) -> MouseModes {
        let mode = self.term.mode();
        let motion = if mode.contains(TermMode::MOUSE_MOTION) {
            MotionReporting::Any
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MotionReporting::Drag
        } else {
            MotionReporting::None
        };
        MouseModes {
            tracking: mode.intersects(TermMode::MOUSE_MODE).then_some(motion),
            encoding: if mode.contains(TermMode::SGR_MOUSE) {
                MouseReportMode::Sgr
            } else {
                MouseReportMode::X10
            },
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        }
    }

    /// Begin a mouse selection at a viewport cell with the given granularity.
    ///
    /// Cell granularity anchors on the exact cell; word and line granularity
    /// resolve their bounds off the live `Term` immediately, so a double- or
    /// triple-click selects something before the pointer has moved at all.
    pub fn begin_selection(&mut self, at: ViewportPoint, mode: SelectionMode) {
        let point = self.selection_point(at);
        match mode {
            SelectionMode::Cell => self.selection.start_cell(point),
            SelectionMode::Word => self.selection.start_word(&self.term, point),
            SelectionMode::Line => self.selection.start_line(&self.term, point),
        }
    }

    /// Extend the active selection to a viewport cell as the pointer drags,
    /// keeping whichever granularity began the gesture. A no-op with no active
    /// selection, so a drag that started off the grid cannot start one.
    pub fn extend_selection(&mut self, at: ViewportPoint) {
        let point = self.selection_point(at);
        self.selection.drag_to(&self.term, point);
    }

    /// Drop the active selection. Returns `true` when there was one to drop, so
    /// the caller can skip a repaint that would change nothing.
    pub fn clear_selection(&mut self) -> bool {
        let had = self.selection.range().is_some();
        self.selection.clear();
        had
    }

    /// Whether a non-empty selection is active on this pane.
    #[must_use]
    pub fn has_selection(&self) -> bool {
        self.selection.range().is_some_and(|range| !range.is_empty())
    }

    /// The selected text, `WRAPLINE`-joined and trailing-space trimmed, or
    /// `None` when nothing is selected.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        self.selection.copy_text(&self.term).map(|text| filter_terminal_image_placeholders(&text))
    }

    /// The active selection projected onto the painted viewport, one span per
    /// visible row. Empty when nothing is selected or the selection scrolled
    /// off screen.
    #[must_use]
    pub fn selection_spans(&self) -> Vec<SelectionSpan> {
        let Some(range) = self.selection.range() else {
            return Vec::new();
        };
        viewport_spans(&range, self.display_offset(), self.content.rows.len(), self.term.columns())
    }

    /// Resolve a viewport cell onto the absolute grid line it reads from, which
    /// is the coordinate space every [`scribe_client::selection`] API
    /// speaks. Shared by the selection gestures and the smart-selection lookup
    /// so a click means the same cell to both.
    fn selection_point(&self, at: ViewportPoint) -> SelectionPoint {
        SelectionPoint { row: self.grid_line_for_viewport_row(at.row), col: at.col }
    }

    /// The smart-selection rules that match at a viewport cell, richest first.
    ///
    /// Only rules that carry at least one action are returned, because the
    /// caller lowers each one onto a context-menu row.
    #[must_use]
    pub fn smart_selection_actions(
        &self,
        rules: &CompiledSmartSelection,
        at: ViewportPoint,
    ) -> Vec<SmartSelectionCandidate> {
        rules.action_candidates_at(&self.term, self.selection_point(at))
    }

    /// The URL, path, or OSC 8 hyperlink under a viewport cell, with the
    /// viewport rows its underline covers.
    ///
    /// Takes `&mut self` because the scan is lazy: the cache rescans here on
    /// the first lookup after the grid moved, so an idle pane pays nothing and
    /// a pointer resting on one cell pays once.
    ///
    /// The row mapping is the plain `row - display_offset` the scanner itself
    /// uses, not [`Self::selection_point`]'s split-scroll-aware one, so the
    /// underline lands on exactly the cells the scan matched. Inside a
    /// split-scroll pin the two disagree and links under the pinned rows are
    /// not offered — the scanner has no notion of the pin either, so hit-testing
    /// them against it would only underline the wrong cells.
    #[must_use]
    pub fn link_at(&mut self, at: ViewportPoint) -> Option<HoveredLink> {
        if self.content.pin_rows > 0 {
            return None;
        }
        self.urls.refresh(&self.term);
        let display_offset = grid_i32(self.display_offset());
        let span = self.urls.url_at(grid_i32(at.row) - display_offset, at.col)?;
        let rows = span
            .segments
            .iter()
            .filter_map(|segment| {
                let row = usize::try_from(segment.row.saturating_add(display_offset)).ok()?;
                (row < self.content.rows.len()).then_some(SelectionSpan {
                    row,
                    start_col: segment.col_start,
                    end_col: segment.col_end,
                })
            })
            .collect();
        Some(HoveredLink { kind: span.kind, target: span.url.clone(), rows })
    }

    /// The grid line a viewport row reads from.
    ///
    /// Outside the split-scroll pin this is just the row shifted by the display
    /// offset. Inside the pin it is a *live* screen row: the pin shows
    /// `[cursor_line - pin_rows + 1, cursor_line]` no matter where the viewport
    /// is scrolled to, which is what keeps an AI tool's prompt composable.
    fn grid_line_for_viewport_row(&self, row: usize) -> i32 {
        let row = grid_i32(row);
        let pin_rows = self.content.pin_rows;
        let top_rows = grid_i32(self.term.screen_lines().saturating_sub(pin_rows));
        if pin_rows > 0 && row >= top_rows {
            grid_i32(self.cursor_line()) - grid_i32(pin_rows.saturating_sub(1)) + (row - top_rows)
        } else {
            row - grid_i32(self.term.grid().display_offset())
        }
    }

    /// The live screen row the shell cursor sits on.
    fn cursor_line(&self) -> usize {
        usize::try_from(self.term.grid().cursor.point.line.0).unwrap_or(0)
    }

    /// How many rows the split-scroll pin should occupy right now.
    ///
    /// Zero unless the config toggle, the AI provider, the scroll position, and
    /// the normal screen buffer all agree — the exact gate
    /// [`split_scroll_eligible`] encodes.
    fn active_pin_rows(&self) -> usize {
        let alt_screen = self.term.mode().contains(TermMode::ALT_SCREEN);
        if !split_scroll_eligible(self.split_scroll, self.display_offset(), alt_screen) {
            return 0;
        }
        let screen_lines = self.term.screen_lines();
        let pin_rows = compute_pin_rows(screen_lines);
        align_pin_rows_to_logical_lines(&self.term, pin_rows, self.cursor_line(), screen_lines)
            .min(screen_lines)
    }

    /// Converts the Alacritty display viewport into fixed-width display rows,
    /// carrying each cell's colour and attribute state to the paint path.
    ///
    /// The viewport is read through the grid's display offset, so a scrolled
    /// pane paints scrollback rather than the live screen. When split-scroll is
    /// active the trailing [`Content::pin_rows`] rows are read from the live
    /// screen instead, anchored on the shell cursor.
    fn make_content(&mut self) {
        // Every path that can move a visible cell — a parse, a scroll, a
        // resize, a history trim, a vi motion — rebuilds the snapshot here, so
        // this is the one place the link scan has to be invalidated from.
        self.urls.mark_dirty();
        let lines = self.term.screen_lines();
        let columns = self.term.columns();
        let display_offset = grid_i32(self.display_offset());
        let pin_rows = self.active_pin_rows();
        let top_rows = lines.saturating_sub(pin_rows);

        let mut rows = Vec::with_capacity(lines);
        for row in 0..top_rows {
            rows.push(Self::read_row(&self.term, grid_i32(row) - display_offset, columns));
        }
        let first_pin_line = grid_i32(self.cursor_line()) - grid_i32(pin_rows.saturating_sub(1));
        for row in 0..pin_rows {
            rows.push(Self::read_row(&self.term, first_pin_line + grid_i32(row), columns));
        }

        let vi_cursor = self.viewport_vi_cursor(top_rows, display_offset);
        let shell_cursor = self.viewport_shell_cursor(lines, columns, pin_rows);
        self.content = Arc::new(Content {
            rows,
            display_offset: self.display_offset(),
            pin_rows,
            vi_cursor,
            shell_cursor,
        });
        self.content_stale = false;
    }

    /// Project the live shell cursor onto the viewport the snapshot just built.
    ///
    /// Ordinary scrollback hides the live cursor. Split-scroll is the
    /// exception: its pinned tail is deliberately live, and `make_content`
    /// places the shell cursor on the pin's final row.
    fn viewport_shell_cursor(
        &self,
        lines: usize,
        columns: usize,
        pin_rows: usize,
    ) -> Option<ShellCursor> {
        let mode = self.term.mode();
        if mode.contains(TermMode::VI) || !mode.contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let style = self.term.cursor_style().shape;
        let shape = match style {
            TerminalCursorShape::Hidden => return None,
            TerminalCursorShape::Beam => ShellCursorShape::Beam,
            TerminalCursorShape::Underline => ShellCursorShape::Underline,
            TerminalCursorShape::Block | TerminalCursorShape::HollowBlock => {
                ShellCursorShape::Block
            }
        };
        let row = if pin_rows > 0 {
            lines.checked_sub(1)?
        } else {
            if self.display_offset() > 0 {
                return None;
            }
            self.cursor_line()
        };
        let col = self.term.grid().cursor.point.column.0;
        (row < lines && col < columns)
            .then_some(ShellCursor { point: ViewportPoint { row, col }, shape })
    }

    /// The vi cursor in viewport coordinates, if it is on a painted scrollback
    /// row. The pin shows live rows, so a vi cursor never lands inside it.
    fn viewport_vi_cursor(&self, top_rows: usize, display_offset: i32) -> Option<ViewportPoint> {
        if !self.is_vi_mode() {
            return None;
        }
        let cursor = vi_mode::vi_cursor(&self.term);
        let row = usize::try_from(cursor.row.checked_add(display_offset)?).ok()?;
        (row < top_rows).then_some(ViewportPoint { row, col: cursor.col })
    }

    /// Read one grid line into display cells, clamped to the addressable range.
    ///
    /// Alacritty's `Grid` only offers a panicking `Index`, and the split-scroll
    /// pin can ask for a line above the scrollback on a nearly empty screen, so
    /// the clamp is what keeps a legal snapshot request from being a panic.
    fn read_row(term: &Term<VoidListener>, line: i32, columns: usize) -> Vec<Cell> {
        let grid = term.grid();
        let line = Line(line.clamp(grid.topmost_line().0, grid.bottommost_line().0));
        (0..columns).map(|column| Self::snapshot_cell(&grid[line][Column(column)])).collect()
    }

    fn snapshot_cell(cell: &alacritty_terminal_gpui::term::cell::Cell) -> Cell {
        let mut zerowidth = ['\0'; 3];
        let mut zerowidth_len = 0u8;
        let marks = cell.zerowidth().unwrap_or_default();
        for (target, mark) in zerowidth.iter_mut().zip(marks.iter().copied()) {
            *target = mark;
            zerowidth_len = zerowidth_len.saturating_add(1);
        }
        Cell {
            c: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
            zerowidth,
            zerowidth_len,
            underline_color: cell.underline_color(),
        }
    }
}

/// Narrow a grid extent to the signed line space Alacritty indexes with.
///
/// Row counts come out of `usize` APIs but grid lines are `i32` (negative rows
/// are scrollback), and no terminal has `i32::MAX` rows, so saturating is the
/// honest conversion.
fn grid_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

impl OutputTarget for DisplayOnlyTerminal {
    fn advance_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        DisplayOnlyTerminal::advance_output(self, bytes)
    }

    fn publish_content(&mut self) {
        DisplayOnlyTerminal::publish_content(self);
    }
}

/// A pane's mutable half: the synchronized-frame queue in front of the grid,
/// and the grid those committed frames advance.
///
/// The two are locked together because they are one pipeline — a committed
/// frame leaves the queue and enters the parser in the same step. Nothing on
/// the paint path takes this lock: a VTE parse holds it for as long as a whole
/// batch of firehose output takes to apply.
pub struct PaneStream {
    /// Committed `CSI ? 2026` bursts waiting to reach the grid.
    pub queue: SyncFrameQueue,
    /// The display grid those bursts advance.
    pub terminal: DisplayOnlyTerminal,
}

impl PaneStream {
    /// Flush queued output, replace the grid, and publish one final snapshot.
    pub fn rebuild(
        &mut self,
        bytes: &[u8],
        columns: usize,
        lines: usize,
        kept_rows: usize,
    ) -> bool {
        let flushed = flush_before_rebuild(&mut self.queue, &mut self.terminal);
        let rebuilt = self.terminal.rebuild(bytes, columns, lines, kept_rows);
        flushed || rebuilt
    }

    /// Whether either half of the pipeline is holding a synchronized update
    /// that needs a timeout flush if its terminator never arrives.
    #[must_use]
    pub fn sync_armed(&self) -> bool {
        self.sync_deadline().is_some()
    }

    /// The nearest raw-frame or parser synchronized-update deadline.
    #[must_use]
    pub fn sync_deadline(&self) -> Option<Instant> {
        match (self.terminal.parser_sync_deadline(), self.queue.raw_sync_deadline()) {
            (Some(parser), Some(raw)) => Some(parser.min(raw)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }
}

/// Everything the paint pass reads off a pane, republished after every change.
///
/// The projection exists so the renderer never waits on a [`PaneStream`]: a
/// firehose holds that lock for the length of a VTE parse, and a frame that had
/// to queue behind one is a dropped frame. Every field is captured from the
/// same grid state in one pass, so a paint can never mix a fresh grid with a
/// stale cursor or selection.
pub struct PaneFrame {
    /// The grid snapshot the pane paints.
    pub content: Arc<Content>,
    /// CPU image scene captured at the same commit as the text projection.
    pub image_scene: Arc<CommittedImageScene>,
    /// Viewport measurements the overlay scrollbar sizes its thumb from.
    pub metrics: ScrollMetrics,
    /// The active selection projected onto the painted viewport.
    pub selection_spans: Vec<SelectionSpan>,
    /// Cursor placement the IME anchors a composition on.
    pub cursor: CursorPlacement,
    /// Viewport geometry in cells, which reconnect reattaches panes at.
    pub dimensions: (usize, usize),
    /// The split-scroll gate the grid was last given, so the shell can tell
    /// whether this projection already reflects the gate it wants to push.
    pub split_scroll: SplitScrollEligibility,
}

impl PaneFrame {
    fn capture(terminal: &DisplayOnlyTerminal) -> Self {
        Self {
            content: terminal.content(),
            image_scene: terminal.image_scene(),
            metrics: terminal.scroll_metrics(),
            selection_spans: terminal.selection_spans(),
            cursor: terminal.cursor_placement(),
            dimensions: terminal.dimensions(),
            split_scroll: terminal.split_scroll,
        }
    }
}

/// One pane: the parse pipeline plus the render projection published from it.
///
/// Held behind an [`Arc`] so a caller resolves the pane under the [`PaneGrids`]
/// lock and then works on it with that lock released — which is what keeps a
/// VTE parse off the registry and out of every other pane's way.
pub struct PaneGrid {
    stream: Mutex<PaneStream>,
    published: Mutex<Arc<PaneFrame>>,
}

impl PaneGrid {
    fn new(columns: usize, lines: usize) -> Self {
        let terminal = DisplayOnlyTerminal::new(columns, lines);
        let published = Arc::new(PaneFrame::capture(&terminal));
        Self {
            stream: Mutex::new(PaneStream { queue: SyncFrameQueue::default(), terminal }),
            published: Mutex::new(published),
        }
    }

    /// Runs `edit` against the pane's parse pipeline and republishes the
    /// projection the paint pass reads.
    ///
    /// Every mutation of the grid goes through here, which is what lets the
    /// projection be republished unconditionally rather than guessing which
    /// edits changed something paintable.
    ///
    /// `None` when the pane's lock is poisoned, matching how the rest of the
    /// client degrades a poisoned pane to a no-op instead of panicking the
    /// thread that touched it.
    pub fn with_stream<R>(&self, edit: impl FnOnce(&mut PaneStream) -> R) -> Option<R> {
        let Ok(mut stream) = self.stream.lock() else {
            tracing::warn!("pane stream mutex poisoned; dropping a grid update");
            return None;
        };
        let result = edit(&mut stream);
        let frame = Arc::new(PaneFrame::capture(&stream.terminal));
        if let Ok(mut published) = self.published.lock() {
            *published = frame;
        }
        Some(result)
    }

    /// Runs `edit` against the pane's grid. Shorthand for the common
    /// [`Self::with_stream`] that never touches the frame queue.
    pub fn with_terminal<R>(&self, edit: impl FnOnce(&mut DisplayOnlyTerminal) -> R) -> Option<R> {
        self.with_stream(|stream| edit(&mut stream.terminal))
    }

    /// The published render projection.
    #[must_use]
    pub fn frame(&self) -> Option<Arc<PaneFrame>> {
        self.published.lock().ok().map(|published| Arc::clone(&published))
    }

    /// The nearest raw-frame or parser synchronized-update deadline.
    ///
    /// Read-only, so it deliberately skips the republish [`Self::with_stream`]
    /// does: the expiry task polls this across every pane.
    #[must_use]
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.stream.lock().ok().and_then(|stream| stream.sync_deadline())
    }

    /// Whether committed bursts remain queued behind the pacer.
    ///
    /// Read-only for the same reason [`Self::sync_deadline`] is: the pacer polls
    /// this across every pane on every frame interval, and a pane with nothing
    /// queued must not pay for a projection republish to say so.
    #[must_use]
    pub fn has_queued_frames(&self) -> bool {
        self.stream.lock().is_ok_and(|stream| stream.queue.has_frames())
    }

    /// Presents this pane's next queued burst, returning how many repaints that
    /// owes.
    ///
    /// One call is one redraw's worth of output. The emptiness check runs first
    /// so the common case — a pane the drain already caught up — costs a read
    /// lock rather than a republished projection.
    pub fn present_next_burst(&self) -> usize {
        if !self.has_queued_frames() {
            return 0;
        }
        self.with_stream(|stream| {
            usize::from(present_queued_burst(&mut stream.queue, &mut stream.terminal))
        })
        .unwrap_or(0)
    }

    /// Commits every synchronized update on this pane whose deadline has
    /// passed, returning how many repaints that owes.
    ///
    /// The parser side goes first so a raw frame flushed in the same pass lands
    /// after the bytes the parser was already holding, which is the order they
    /// arrived in. The flushed frame is presented under the same pacing every
    /// other committed burst is, so an expiry cannot jump the queue ahead of the
    /// frames already waiting on it.
    pub fn flush_expired_sync(&self, now: Instant) -> usize {
        self.with_stream(|stream| {
            let mut redraws = usize::from(stream.terminal.flush_parser_sync_timeout(now));
            if stream.queue.flush_raw_timeout(now) {
                redraws +=
                    usize::from(present_queued_burst(&mut stream.queue, &mut stream.terminal));
            }
            redraws
        })
        .unwrap_or(0)
    }
}

/// One display grid per session, so a split window paints each pane from its
/// own terminal state instead of interleaving every pane into one grid.
///
/// Grids are created lazily: the first output batch for a session mints one at
/// the window's default geometry, and the pane layout resizes it as soon as it
/// knows how much of the window that session owns.
///
/// The registry's own lock guards the map, never a parse: callers take an
/// [`Arc<PaneGrid>`] out of it and release the registry before touching the
/// pane. Forgetting a session therefore never waits on the batch being parsed
/// into it — the removed handle simply outlives the map entry.
pub struct PaneGrids {
    /// Geometry a freshly minted grid starts at, before the pane layout has
    /// published a per-pane size for its session.
    default_columns: usize,
    default_lines: usize,
    grids: HashMap<SessionId, Arc<PaneGrid>>,
}

impl PaneGrids {
    /// Create an empty set whose grids default to `columns` x `lines`.
    pub fn new(columns: usize, lines: usize) -> Self {
        Self { default_columns: columns, default_lines: lines, grids: HashMap::new() }
    }

    /// Take a handle on `session_id`'s pane, creating it at the default
    /// geometry. Release the registry lock before using the handle.
    pub fn pane(&mut self, session_id: SessionId) -> Arc<PaneGrid> {
        let (columns, lines) = (self.default_columns, self.default_lines);
        Arc::clone(
            self.grids.entry(session_id).or_insert_with(|| Arc::new(PaneGrid::new(columns, lines))),
        )
    }

    /// Every live pane, so a caller can walk them all with the registry lock
    /// already released.
    #[must_use]
    pub fn panes(&self) -> Vec<Arc<PaneGrid>> {
        self.grids.values().map(Arc::clone).collect()
    }

    /// The published projection for `session_id`, or `None` when no output has
    /// ever reached that session's pane.
    #[must_use]
    pub fn frame(&self, session_id: SessionId) -> Option<Arc<PaneFrame>> {
        self.grids.get(&session_id).and_then(|pane| pane.frame())
    }

    /// Current viewport geometry for `session_id`, when its grid exists.
    #[must_use]
    pub fn dimensions(&self, session_id: SessionId) -> Option<(usize, usize)> {
        self.frame(session_id).map(|frame| frame.dimensions)
    }

    /// The scrollbar viewport metrics for `session_id`, or `None` when that
    /// session has no grid yet (nothing to scroll, so nothing to draw).
    pub fn scroll_metrics(&self, session_id: SessionId) -> Option<ScrollMetrics> {
        self.frame(session_id).map(|frame| frame.metrics)
    }

    /// Drop an exited session's grid.
    pub fn forget(&mut self, session_id: SessionId) {
        self.grids.remove(&session_id);
    }
}

#[cfg(test)]
mod tests {
    use scribe_common::config::SmartSelectionConfig;
    use scribe_common::protocol::PromptMarkKind;

    use super::*;
    use crate::session_lifecycle::{CommandMark, CommandStatus, PromptMarks};
    use crate::sync_frames::{SyncFrameQueue, drain_all_committed};

    /// A terminal holding `lines` numbered rows, the last one unterminated so
    /// the shell cursor sits on it exactly as it would after a prompt.
    fn terminal_with_numbered_lines(
        columns: usize,
        screen_lines: usize,
        lines: usize,
    ) -> DisplayOnlyTerminal {
        let mut terminal = DisplayOnlyTerminal::new(columns, screen_lines);
        let body = (1..=lines).map(|n| format!("l{n:02}")).collect::<Vec<_>>().join("\r\n");
        terminal.feed_output(body.as_bytes());
        terminal
    }

    fn pinned() -> SplitScrollEligibility {
        SplitScrollEligibility { scroll_pin_enabled: true, ai_provider_enabled: true }
    }

    // @lat: [[test#GPUI Terminal Viewport#Link lookup follows the scrolled viewport]]
    #[gpui::test]
    fn link_lookup_follows_the_scrolled_viewport() {
        let mut terminal = DisplayOnlyTerminal::new(40, 3);
        terminal.feed_output(b"see https://example.com/a\r\nrun ./build.sh\r\nplain");

        // Row 0 is the URL line: every cell of the URL resolves to one span
        // carrying the whole thing, and the words around it resolve to nothing.
        let link = terminal.link_at(ViewportPoint { row: 0, col: 6 }).expect("URL under the cell");
        assert!(matches!(link.kind, SpanKind::Url));
        assert_eq!(link.target, "https://example.com/a");
        assert_eq!(link.rows, vec![SelectionSpan { row: 0, start_col: 4, end_col: 24 }]);
        assert!(terminal.link_at(ViewportPoint { row: 0, col: 1 }).is_none());

        // A relative path is a Path, not a URL: the two open through different
        // routes, and only one of them is resolved against the pane's CWD.
        let path = terminal.link_at(ViewportPoint { row: 1, col: 6 }).expect("path under the cell");
        assert!(matches!(path.kind, SpanKind::Path));
        assert_eq!(path.target, "./build.sh");
        assert_eq!(path.rows, vec![SelectionSpan { row: 1, start_col: 4, end_col: 13 }]);

        // Push both lines into scrollback, then scroll one row back up. The same
        // viewport row now paints different content, so the lookup has to answer
        // for what is there now — a cache that outlived the scroll would still
        // hand back the URL that used to be on row 0.
        terminal.feed_output(b"\r\nmore\r\nlines");
        assert!(terminal.link_at(ViewportPoint { row: 0, col: 6 }).is_none());
        assert!(terminal.scroll(Scroll::Delta(1)));
        let scrolled =
            terminal.link_at(ViewportPoint { row: 0, col: 6 }).expect("path after the scroll");
        assert_eq!(scrolled.target, "./build.sh");
        assert_eq!(scrolled.rows, vec![SelectionSpan { row: 0, start_col: 4, end_col: 13 }]);
    }

    // @lat: [[test#GPUI Terminal Viewport#Scrolling paints scrollback and returns to the live bottom]]
    #[gpui::test]
    fn scrolling_paints_scrollback_and_returns_to_the_bottom() {
        let mut terminal = terminal_with_numbered_lines(20, 3, 5);
        // The unscrolled viewport is the live tail.
        assert_eq!(terminal.display_offset(), 0);
        assert_eq!(terminal.content().row_text(0).trim_end(), "l03");

        assert!(terminal.scroll(Scroll::Delta(2)));
        assert_eq!(terminal.display_offset(), 2);
        // Only a snapshot that honours the display offset can show these.
        assert_eq!(terminal.content().row_text(0).trim_end(), "l01");
        assert_eq!(terminal.content().row_text(2).trim_end(), "l03");

        // Scrolling past the oldest row is a no-op rather than a move.
        assert!(!terminal.scroll(Scroll::Delta(5)));

        assert!(terminal.scroll(Scroll::Bottom));
        assert_eq!(terminal.display_offset(), 0);
        assert_eq!(terminal.content().row_text(0).trim_end(), "l03");
    }

    // @lat: [[test#GPUI Terminal Viewport#Prompt marks anchor and scroll in absolute rows]]
    #[gpui::test]
    fn prompt_marks_anchor_and_scroll_in_absolute_rows() {
        let mut terminal = terminal_with_numbered_lines(20, 3, 5);
        // Five lines through a three-row screen leaves two rows of history and
        // the cursor on the last screen row.
        let anchor = terminal.prompt_anchor();
        assert_eq!(anchor.history, 2);
        assert_eq!(anchor.screen_lines, 3);
        assert_eq!(anchor.history + anchor.cursor_row, 4);
        // At the live bottom the viewport top is the first row after history.
        assert_eq!(terminal.viewport_top_abs(), 2);

        // Jumping to the absolute row of the oldest line puts it on top.
        assert!(terminal.scroll_to_abs(0));
        assert_eq!(terminal.viewport_top_abs(), 0);
        assert_eq!(terminal.content().row_text(0).trim_end(), "l01");
        // Re-issuing the same jump is a no-op rather than a redundant repaint.
        assert!(!terminal.scroll_to_abs(0));
        // And jumping back down lands on the row that mark named.
        assert!(terminal.scroll_to_abs(2));
        assert_eq!(terminal.content().row_text(0).trim_end(), "l03");
    }

    // @lat: [[test#GPUI Terminal Viewport#Scrollback trim drops rows and shifts marks]]
    #[gpui::test]
    fn scrollback_trim_drops_rows_and_shifts_marks() {
        let mut terminal = terminal_with_numbered_lines(20, 3, 12);
        // Twelve lines through a three-row screen leaves nine rows of history.
        assert_eq!(terminal.history_size(), 9);
        let mut marks = PromptMarks::new();
        let session = SessionId::new();
        for row in [2usize, 6, 8] {
            marks.record(
                session,
                PromptMarkKind::PromptStart,
                None,
                PromptAnchor { history: row, screen_lines: 3, cursor_row: 0, cursor_col: 0 },
            );
        }

        // Trimming back to four rows drops the five oldest, and the surviving
        // rows really are gone from the grid, not merely renumbered.
        let dropped = terminal.trim_history(4);
        assert_eq!(dropped, 5);
        assert_eq!(terminal.history_size(), 4);
        assert_eq!(terminal.scroll_metrics().history_size, 4);
        terminal.scroll_to_abs(0);
        assert_eq!(terminal.content().row_text(0).trim_end(), "l06");

        // The two marks below the cut shift down by the drop; the one inside it
        // (row 2 of 9, now gone) is retired, because the row it named no longer
        // exists to jump to or tick.
        marks.on_trim(session, dropped);
        assert_eq!(
            marks.marks(session),
            [
                CommandMark { abs_pos: 1, status: CommandStatus::Unknown },
                CommandMark { abs_pos: 3, status: CommandStatus::Unknown },
            ]
        );

        // A trim that keeps everything the grid already holds is a no-op.
        assert_eq!(terminal.trim_history(9), 0);
    }

    // @lat: [[test#GPUI Terminal Viewport#Split-scroll pins the live rows under the scrollback]]
    #[gpui::test]
    fn split_scroll_pins_live_rows_under_scrollback() {
        let mut terminal = terminal_with_numbered_lines(20, 8, 12);
        terminal.scroll(Scroll::Delta(4));
        // The gate is closed until the shell says the pane is an eligible AI
        // pane, so a plain scrolled pane paints one contiguous region.
        assert_eq!(terminal.pin_rows(), 0);
        assert_eq!(terminal.content().row_text(7).trim_end(), "l08");

        assert!(terminal.set_split_scroll_eligibility(pinned()));
        let content = terminal.content();
        assert_eq!(content.pin_rows, 5);
        // Top three rows are scrollback at the current offset...
        assert_eq!(content.row_text(0).trim_end(), "l01");
        assert_eq!(content.row_text(2).trim_end(), "l03");
        // ...and the pinned tail is the live screen, ending on the cursor row,
        // which is what keeps an AI tool's prompt composable while scrolled.
        assert_eq!(content.row_text(3).trim_end(), "l08");
        assert_eq!(content.row_text(7).trim_end(), "l12");

        // Closing the gate restores the single scrolled region.
        assert!(terminal.set_split_scroll_eligibility(SplitScrollEligibility::default()));
        assert_eq!(terminal.pin_rows(), 0);
        assert_eq!(terminal.content().row_text(7).trim_end(), "l08");
    }

    // @lat: [[test#GPUI Terminal Viewport#Vi mode publishes a cursor the paint path can draw]]
    #[gpui::test]
    fn vi_mode_publishes_a_viewport_cursor() {
        let mut terminal = terminal_with_numbered_lines(20, 4, 3);
        assert!(!terminal.is_vi_mode());
        assert!(terminal.content().vi_cursor.is_none());

        terminal.toggle_vi_mode();
        assert!(terminal.is_vi_mode());
        let start = terminal.content().vi_cursor.expect("vi mode publishes a cursor");

        terminal.vi_motion(ViMotion::Up);
        let moved = terminal.content().vi_cursor.expect("the cursor survives a motion");
        assert_eq!(moved.row + 1, start.row);
        assert_eq!(moved.col, start.col);

        terminal.toggle_vi_mode();
        assert!(terminal.content().vi_cursor.is_none());
    }

    // @lat: [[test#GPUI Terminal Viewport#Smart selection resolves through the scrolled viewport]]
    #[gpui::test]
    fn smart_selection_resolves_through_the_scrolled_viewport() {
        let rules = CompiledSmartSelection::compile(&SmartSelectionConfig::default());
        let mut terminal = DisplayOnlyTerminal::new(60, 3);
        terminal.feed_output(b"visit https://example.com/spec now\r\ntwo\r\nthree\r\nfour");

        // The URL scrolled into history; addressing it needs the viewport row
        // to be resolved against the display offset, not the live screen.
        assert!(terminal.scroll(Scroll::Delta(1)));
        let hits = terminal.smart_selection_actions(&rules, ViewportPoint { row: 0, col: 10 });
        let uri = hits.iter().find(|hit| hit.rule_name == "URI").expect("the URI rule matched");
        assert_eq!(uri.text, "https://example.com/spec");

        // Blank space matches no actionable rule, so an ordinary right-click
        // over an empty pane still gets a plain menu.
        assert!(
            terminal.smart_selection_actions(&rules, ViewportPoint { row: 2, col: 50 }).is_empty()
        );
    }

    // @lat: [[test#GPUI Sync Frame Queue#Split sync frame reaches terminal whole]]
    #[gpui::test]
    fn split_sync_frame_renders_committed_content() {
        let mut terminal = DisplayOnlyTerminal::new(40, 3);
        let mut queue = SyncFrameQueue::default();

        // A synchronized-update frame chunked across four IPC messages: the
        // BSU escape is split mid-sequence, the body straddles two messages,
        // and the ESU arrives last. The terminal must never see a torn frame.
        queue.queue_output_frames(b"\x1b[?20");
        queue.queue_output_frames(b"26hhello");
        queue.queue_output_frames(b" world");
        queue.queue_output_frames(b"\x1b[?2026l");

        let summary = drain_all_committed(&mut queue, &mut terminal);
        assert!(summary.needs_redraw);
        assert!(terminal.visible_text().starts_with("hello world"));
        assert!(terminal.parser_sync_deadline().is_none());
    }

    // @lat: [[test#GPUI Sync Frame Queue#Advancing a frame defers the snapshot]]
    #[gpui::test]
    fn advancing_frames_holds_the_snapshot_until_published() {
        let mut terminal = DisplayOnlyTerminal::new(20, 4);
        terminal.feed_output(b"first");
        let published = terminal.content();

        // Two frames the pacer would drain through on its way to the third:
        // the grid takes them, the snapshot every reader holds does not move.
        terminal.advance_output(b"\r\nsecond");
        terminal.advance_output(b"\r\nthird");
        assert!(Arc::ptr_eq(&published, &terminal.content()), "no rebuild for a skipped frame");
        assert_eq!(published.row_text(1).trim_end(), "");

        // Publishing once catches the snapshot up to everything advanced since
        // the last one, and a second publish has nothing left to rebuild.
        assert!(terminal.publish_content());
        assert_eq!(terminal.content().row_text(2).trim_end(), "third");
        assert!(!terminal.publish_content(), "a current snapshot is not rebuilt again");
    }

    // @lat: [[test#GPUI Terminal Viewport#A parse in flight blocks neither the registry nor a paint]]
    #[gpui::test]
    fn a_held_pane_stream_blocks_neither_the_registry_nor_a_paint() {
        let mut grids = PaneGrids::new(20, 2);
        let (busy, idle) = (SessionId::new(), SessionId::new());
        let (busy_pane, idle_pane) = (grids.pane(busy), grids.pane(idle));
        idle_pane.with_terminal(|terminal| terminal.feed_output(b"idle"));
        busy_pane.with_terminal(|terminal| terminal.feed_output(b"before"));

        // Stand in for a batch mid-parse: the busy pane's stream is held for as
        // long as the parse would take.
        let parsing = busy_pane.stream.lock().expect("stream lock");

        // The registry itself is still free, so a batch bound for another pane
        // — and every paint-path read, which resolves through the registry —
        // keeps running.
        assert!(grids.frame(idle).is_some_and(|frame| frame.content.text().starts_with("idle")));
        idle_pane.with_terminal(|terminal| terminal.feed_output(b"!"));

        // Even the busy pane still paints: the projection published before the
        // parse started is what a frame reads, not the grid under the lock.
        let published = grids.frame(busy).expect("busy pane projection");
        assert!(published.content.text().starts_with("before"));

        drop(parsing);
    }

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Snapshot carries per-cell colour and attributes]]
    #[gpui::test]
    fn content_snapshot_carries_sgr_state() {
        use vte::ansi::NamedColor;

        let mut terminal = DisplayOnlyTerminal::new(20, 2);
        // Bold red on blue, then a true-colour underlined run, then a reset.
        terminal.feed_output(b"\x1b[1;31;44mA\x1b[0m\x1b[4;38;2;10;20;30mB\x1b[0mC");

        let content = terminal.content();
        let row = content.rows.first().expect("first row");
        let a = row.first().copied().expect("cell A");
        assert_eq!(a.c, 'A');
        assert_eq!(a.fg, Color::Named(NamedColor::Red));
        assert_eq!(a.bg, Color::Named(NamedColor::Blue));
        assert!(a.flags.contains(Flags::BOLD));

        let b = row.get(1).copied().expect("cell B");
        assert_eq!(b.c, 'B');
        assert_eq!(b.fg, Color::Spec(vte::ansi::Rgb { r: 10, g: 20, b: 30 }));
        assert!(b.flags.contains(Flags::UNDERLINE));
        assert!(!b.flags.contains(Flags::BOLD));

        let c = row.get(2).copied().expect("cell C");
        assert_eq!(c.c, 'C');
        assert_eq!(c.fg, Color::Named(NamedColor::Foreground));
        assert_eq!(c.bg, Color::Named(NamedColor::Background));
        assert!(c.flags.is_empty());

        // Blank cells still fill the row so every row keeps terminal width.
        assert_eq!(row.len(), 20);
        assert_eq!(content.row_text(0).trim_end(), "ABC");
    }

    // @lat: [[test#GPUI Sync Frame Queue#Flushes parser sync update on expiry]]
    #[gpui::test]
    fn parser_sync_update_flushes_after_expiry() {
        let mut terminal = DisplayOnlyTerminal::new(40, 3);

        // A committed frame that opens but never closes a synchronized update
        // arms the parser's own 150 ms timeout and holds the payload back.
        terminal.feed_output(b"\x1b[?2026hheld");
        let deadline = terminal.parser_sync_deadline().expect("parser sync armed");
        assert!(!terminal.visible_text().starts_with("held"));

        // Flushing at the deadline commits the buffered bytes and clears the
        // parser timeout so the drain stops waking for it.
        assert!(terminal.flush_parser_sync_timeout(deadline));
        assert!(terminal.parser_sync_deadline().is_none());
        assert!(terminal.visible_text().starts_with("held"));
    }
}
