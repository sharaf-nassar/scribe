//! Display-only terminal state adapted from Zed's terminal model.
//!
//! The state owns no PTY. It receives bytes from Scribe IPC, advances Zed's
//! Alacritty fork, and exposes a render-ready grid snapshot to GPUI.

use std::time::Instant;

pub use alacritty_terminal_gpui::term::cell::Flags;
use alacritty_terminal_gpui::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line},
    term::{Config, Osc52, Term},
};
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

/// Immutable grid snapshot consumed by [`crate::terminal_element::TerminalElement`].
#[derive(Clone, Default)]
pub struct Content {
    /// Visible rows, including blank cells so every row keeps terminal width.
    pub rows: Vec<Vec<Cell>>,
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

    /// Converts the Alacritty active viewport into fixed-width display rows,
    /// carrying each cell's colour and attribute state to the paint path.
    fn make_content(&mut self) {
        let lines = self.term.screen_lines();
        let columns = self.term.columns();
        self.content.rows = (0..lines)
            .filter_map(|line| {
                let line = i32::try_from(line).ok()?;
                Some(
                    (0..columns)
                        .map(|column| {
                            let cell = &self.term.grid()[Line(line)][Column(column)];
                            Cell { c: cell.c, fg: cell.fg, bg: cell.bg, flags: cell.flags }
                        })
                        .collect(),
                )
            })
            .collect();
    }
}

impl OutputTarget for DisplayOnlyTerminal {
    fn feed_output(&mut self, bytes: &[u8]) -> FeedOutputResult {
        DisplayOnlyTerminal::feed_output(self, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_frames::{SyncFrameQueue, drain_all_committed};

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
