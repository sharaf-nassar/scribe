//! Display-only terminal state adapted from Zed's terminal model.
//!
//! The state owns no PTY. It receives bytes from Scribe IPC, advances Zed's
//! Alacritty fork, and exposes a render-ready grid snapshot to GPUI.
//!
//! It is also the shell's single seam onto the ported terminal-navigation
//! modules, because they all need the live `Term` this type owns: the viewport
//! is scrolled here ([`DisplayOnlyTerminal::scroll`]), vi / copy mode is
//! toggled and driven here through [`scribe_client_gpui::vi_mode`], the
//! split-scroll pin is folded into the snapshot through
//! [`scribe_client_gpui::split_scroll`], and a click resolves its
//! [`scribe_client_gpui::smart_selection`] candidates here.

use std::time::Instant;

pub use alacritty_terminal_gpui::grid::Scroll;
pub use alacritty_terminal_gpui::term::cell::Flags;
use alacritty_terminal_gpui::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line},
    term::{Config, Osc52, Term, TermMode},
};
use scribe_client_gpui::selection::SelectionPoint;
use scribe_client_gpui::smart_selection::{CompiledSmartSelection, SmartSelectionCandidate};
use scribe_client_gpui::split_scroll::{
    SplitScrollEligibility, align_pin_rows_to_logical_lines, compute_pin_rows,
    split_scroll_eligible,
};
use scribe_client_gpui::vi_mode::{self, ViMotion};
use vte::ansi::Color;

use crate::sync_frames::{FeedOutputResult, OutputTarget};

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
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Named(vte::ansi::NamedColor::Foreground),
            bg: Color::Named(vte::ansi::NamedColor::Background),
            flags: Flags::empty(),
        }
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

/// Immutable grid snapshot consumed by [`crate::terminal_element::TerminalElement`].
#[derive(Clone, Default)]
pub struct Content {
    /// Visible rows, including blank cells so every row keeps terminal width.
    pub rows: Vec<Vec<Cell>>,
    /// How many trailing rows of [`Self::rows`] show the *live* screen while
    /// the rows above them show scrollback — the split-scroll pin. `0` whenever
    /// split-scroll is inactive, which is the ordinary case.
    pub pin_rows: usize,
    /// Where the vi / copy-mode cursor sits in this snapshot, or `None` when vi
    /// mode is off or the cursor scrolled out of the painted viewport.
    pub vi_cursor: Option<ViewportPoint>,
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
pub struct DisplayOnlyTerminal {
    term: Term<VoidListener>,
    output_processor: vte::ansi::Processor,
    content: Content,
    /// Config + AI-provider gate for split-scroll, pushed in by the shell on
    /// every frame. The live half of the decision (scrolled up, normal screen)
    /// is read off the terminal itself in [`Self::active_pin_rows`].
    split_scroll: SplitScrollEligibility,
}

impl DisplayOnlyTerminal {
    /// Creates an empty terminal at the dimensions sent with `AttachSessions`.
    pub fn new(columns: usize, lines: usize) -> Self {
        let dimensions = TerminalDimensions { columns, lines };
        let config = Config { kitty_keyboard: true, osc52: Osc52::Disabled, ..Config::default() };
        let term = Term::new(config, &dimensions, VoidListener);
        let mut terminal = Self {
            term,
            output_processor: vte::ansi::Processor::new(),
            content: Content::default(),
            split_scroll: SplitScrollEligibility::default(),
        };
        terminal.make_content();
        terminal
    }

    /// Advances one committed frame and reports whether it changed visible
    /// state and whether a synchronized update is still buffering in the
    /// parser. The content snapshot is rebuilt only when the bytes were not
    /// wholly absorbed by an open synchronized update, mirroring the winit
    /// client's `Pane::feed_output`.
    pub fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        self.output_processor.advance(&mut self.term, bytes);
        let needs_redraw = self.output_processor.sync_bytes_count() < bytes.len();
        if needs_redraw {
            self.make_content();
        }
        FeedOutputResult { needs_redraw, sync_pending: self.parser_sync_deadline().is_some() }
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
    pub fn content(&self) -> Content {
        self.content.clone()
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
        let before = self.term.grid().display_offset();
        self.term.scroll_display(scroll);
        let changed = self.term.grid().display_offset() != before;
        if changed {
            self.make_content();
        }
        changed
    }

    /// How far the viewport is scrolled into the scrollback, in rows.
    #[must_use]
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
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
    pub const fn pin_rows(&self) -> usize {
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
        let point = SelectionPoint { row: self.grid_line_for_viewport_row(at.row), col: at.col };
        rules.action_candidates_at(&self.term, point)
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

        self.content.rows = rows;
        self.content.pin_rows = pin_rows;
        self.content.vi_cursor = self.viewport_vi_cursor(top_rows, display_offset);
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
        (0..columns)
            .map(|column| {
                let cell = &grid[line][Column(column)];
                Cell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags }
            })
            .collect()
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
    fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        DisplayOnlyTerminal::feed_output(self, bytes)
    }
}

#[cfg(test)]
mod tests {
    use scribe_common::config::SmartSelectionConfig;

    use super::*;
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
