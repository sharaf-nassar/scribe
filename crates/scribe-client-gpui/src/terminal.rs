//! Display-only terminal state adapted from Zed's terminal model.
//!
//! The state owns no PTY. It receives bytes from Scribe IPC, advances Zed's
//! Alacritty fork, and exposes a render-ready grid snapshot to GPUI.

use alacritty_terminal_gpui::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line},
    term::{Config, Osc52, Term},
};

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

    /// Advances server output and refreshes the immutable content snapshot.
    pub fn write_output(&mut self, bytes: &[u8]) {
        self.output_processor.advance(&mut self.term, bytes);
        self.make_content();
    }

    /// Returns the content captured after the most recent output frame.
    pub fn content(&self) -> Content {
        self.content.clone()
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
