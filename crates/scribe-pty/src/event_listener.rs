use alacritty_terminal::event::{Event, EventListener};
use scribe_common::ids::SessionId;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::metadata::MetadataEvent;

/// Bridges `alacritty_terminal` [`Event`]s into a [`MetadataEvent`] channel.
///
/// The terminal emulator fires events on this listener as it processes escape
/// sequences. We forward the subset that map to metadata events and log the
/// rest for observability.
pub struct ScribeEventListener {
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<MetadataEvent>,
}

impl ScribeEventListener {
    /// Create a new listener that forwards events to `event_tx`.
    #[must_use]
    pub fn new(session_id: SessionId, event_tx: mpsc::UnboundedSender<MetadataEvent>) -> Self {
        Self { session_id, event_tx }
    }

    /// Session this listener belongs to.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Send a `MetadataEvent` on the channel, logging if the receiver has dropped.
    fn emit(&self, event: MetadataEvent) {
        if self.event_tx.send(event).is_err() {
            debug!(
                session_id = %self.session_id,
                "MetadataEvent dropped: receiver closed"
            );
        }
    }
}

impl EventListener for ScribeEventListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                self.emit(MetadataEvent::TitleChanged(title));
            }

            Event::ResetTitle => {
                self.emit(MetadataEvent::TitleChanged(String::new()));
            }

            Event::Bell => {
                self.emit(MetadataEvent::Bell);
            }

            Event::ClipboardStore(kind, text) => {
                // We don't manage a clipboard — log for future integration.
                debug!(
                    session_id = %self.session_id,
                    clipboard_type = ?kind,
                    len = text.len(),
                    "ClipboardStore: clipboard integration not yet implemented"
                );
            }

            Event::ClipboardLoad(kind, formatter) => {
                // Reply with an empty string so the child process doesn't block.
                // A real clipboard integration would call formatter(clipboard_contents).
                let response = formatter("");
                debug!(
                    session_id = %self.session_id,
                    clipboard_type = ?kind,
                    response_len = response.len(),
                    "ClipboardLoad: replying with empty clipboard (not yet implemented)"
                );
                // NOTE: The response would normally be written back to the PTY via a
                // Notify channel. We log it here; Task 7's read loop will wire the
                // Notify path and can use a separate mechanism for write-back.
            }

            Event::PtyWrite(data) => {
                // Terminal-initiated write-back (e.g., DA response, color query reply).
                // Task 7 will route these through the PTY write path.
                debug!(
                    session_id = %self.session_id,
                    len = data.len(),
                    "PtyWrite request from Term (not yet forwarded to PTY)"
                );
            }

            Event::ColorRequest(index, formatter) => {
                // Color query — reply with a default black (#000000).
                let response = formatter(alacritty_terminal::vte::ansi::Rgb { r: 0, g: 0, b: 0 });
                debug!(
                    session_id = %self.session_id,
                    color_index = index,
                    response_len = response.len(),
                    "ColorRequest: replying with default black (not yet implemented)"
                );
            }

            Event::TextAreaSizeRequest(formatter) => {
                // Size query — reply with a zero-size placeholder.
                let response = formatter(alacritty_terminal::event::WindowSize {
                    num_lines: 0,
                    num_cols: 0,
                    cell_width: 0,
                    cell_height: 0,
                });
                debug!(
                    session_id = %self.session_id,
                    response_len = response.len(),
                    "TextAreaSizeRequest: replying with zero size (not yet implemented)"
                );
            }

            // The following events are UI/renderer signals that don't map to metadata.
            Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::Wakeup => {
                // Renderer-facing signals — no metadata to extract.
            }

            Event::Exit => {
                warn!(
                    session_id = %self.session_id,
                    "Term emitted Exit event (PTY closed)"
                );
            }

            Event::ChildExit(status) => {
                debug!(
                    session_id = %self.session_id,
                    ?status,
                    "Child process exited"
                );
            }
        }
    }
}
