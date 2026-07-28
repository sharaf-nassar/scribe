//! Vi / copy-mode keyboard navigation over the terminal grid.
//!
//! Thin parity wrapper over the Alacritty fork's built-in vi mode, matching
//! Zed's `terminal.rs`: toggling vi mode moves a keyboard cursor independent of
//! the shell, `vi_motion` drives it with Alacritty's motion vocabulary, and the
//! cursor position is exposed as a [`SelectionPoint`] so the paint path can
//! highlight it the same way it highlights a mouse selection.

use alacritty_terminal_gpui::Term;
use alacritty_terminal_gpui::event::VoidListener;
use alacritty_terminal_gpui::term::TermMode;

pub use alacritty_terminal_gpui::vi_mode::ViMotion;

use crate::selection::SelectionPoint;

/// `true` when the terminal is currently in vi / copy mode.
pub fn is_vi_mode(term: &Term<VoidListener>) -> bool {
    term.mode().contains(TermMode::VI)
}

/// Toggle vi / copy mode. Entering seeds the vi cursor at the shell cursor;
/// leaving returns control to the live shell.
pub fn toggle_vi_mode(term: &mut Term<VoidListener>) {
    term.toggle_vi_mode();
}

/// Drive the vi cursor with `motion`. A no-op unless vi mode is active, exactly
/// as the fork enforces internally.
pub fn vi_motion(term: &mut Term<VoidListener>, motion: ViMotion) {
    term.vi_motion(motion);
}

/// Current vi-cursor position in absolute grid coordinates (row 0 = viewport
/// top, negative rows = scrollback).
pub fn vi_cursor(term: &Term<VoidListener>) -> SelectionPoint {
    let point = term.vi_mode_cursor.point;
    SelectionPoint { row: point.line.0, col: point.column.0 }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal_gpui::event::VoidListener;
    use alacritty_terminal_gpui::grid::Dimensions;
    use alacritty_terminal_gpui::term::{Config, Term};
    use vte::ansi::Processor;

    use super::{ViMotion, is_vi_mode, toggle_vi_mode, vi_cursor, vi_motion};

    #[derive(Clone, Copy)]
    struct TestDims {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestDims {
        fn total_lines(&self) -> usize {
            self.rows
        }

        fn screen_lines(&self) -> usize {
            self.rows
        }

        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term_with_output(cols: usize, rows: usize, output: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TestDims { cols, rows }, VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, output);
        term
    }

    // @lat: [[test#GPUI Terminal Selection#Vi mode toggles and moves the cursor]]
    #[gpui::test]
    fn vi_mode_toggles_and_moves_cursor() {
        let mut term = term_with_output(20, 4, b"line one\r\nline two");
        assert!(!is_vi_mode(&term));

        // Motions are no-ops until vi mode is active.
        vi_motion(&mut term, ViMotion::Down);
        assert!(!is_vi_mode(&term));

        toggle_vi_mode(&mut term);
        assert!(is_vi_mode(&term));
        let start = vi_cursor(&term);

        vi_motion(&mut term, ViMotion::Right);
        let moved = vi_cursor(&term);
        assert_eq!(moved.row, start.row);
        assert_eq!(moved.col, start.col + 1);

        toggle_vi_mode(&mut term);
        assert!(!is_vi_mode(&term));
    }
}
