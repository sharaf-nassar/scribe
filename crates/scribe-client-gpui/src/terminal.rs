//! Display-only terminal state adapted from Zed's terminal model.
//!
//! The state owns no PTY. It receives bytes from Scribe IPC, advances Zed's
//! Alacritty fork, and exposes a render-ready grid snapshot to GPUI.

use std::time::Instant;

use alacritty_terminal_gpui::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line},
    term::{Config, Osc52, Term},
};

use crate::sync_frames::{FeedOutputResult, OutputTarget};

/// Immutable grid snapshot consumed by [`crate::terminal_element::TerminalElement`].
#[derive(Clone, Default)]
pub struct Content {
    /// Visible rows, including blank cells so every row keeps terminal width.
    pub rows: Vec<String>,
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
        self.content.rows.join("\n")
    }

    /// Converts the Alacritty active viewport into fixed-width display rows.
    fn make_content(&mut self) {
        let lines = self.term.screen_lines();
        let columns = self.term.columns();
        self.content.rows = (0..lines)
            .filter_map(|line| {
                let line = i32::try_from(line).ok()?;
                Some(
                    (0..columns)
                        .map(|column| self.term.grid()[Line(line)][Column(column)].c)
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
