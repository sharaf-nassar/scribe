//! GPUI paint path for a display-only terminal [`Content`](crate::terminal::Content) snapshot.

use gpui::{div, prelude::*, rgb};

use crate::terminal::Content;

/// Paints the current terminal grid with fixed-width rows.
pub struct TerminalElement {
    content: Content,
}

impl TerminalElement {
    /// Captures one stable terminal snapshot for this render pass.
    pub const fn new(content: Content) -> Self {
        Self { content }
    }

    /// Builds the GPUI element tree for the visible terminal grid.
    pub fn paint(self) -> impl IntoElement {
        div()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x0010_1318))
            .text_color(rgb(0x00d9_dde3))
            .font_family("monospace")
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .children(self.content.rows.into_iter().map(|row| div().h_5().child(row))),
            )
    }
}
