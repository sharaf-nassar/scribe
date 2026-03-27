use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::term::ClipboardType;
use alacritty_terminal::vte::ansi::Rgb;
use scribe_common::ids::SessionId;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::metadata::MetadataEvent;

type ClipboardFormatter = std::sync::Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;
type ColorFormatter = std::sync::Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>;
type TextAreaSizeFormatter = std::sync::Arc<dyn Fn(WindowSize) -> String + Sync + Send + 'static>;

/// Events emitted by the terminal core that require server handling.
pub enum SessionEvent {
    Metadata(MetadataEvent),
    ClipboardStore(ClipboardType, String),
    ClipboardLoad(ClipboardType, ClipboardFormatter),
    ColorRequest(usize, ColorFormatter),
    PtyWrite(String),
    TextAreaSizeRequest(TextAreaSizeFormatter),
}

/// Bridges `alacritty_terminal` [`Event`]s into a [`MetadataEvent`] channel.
///
/// The terminal emulator fires events on this listener as it processes escape
/// sequences. We forward the subset that map to metadata events and log the
/// rest for observability.
pub struct ScribeEventListener {
    session_id: SessionId,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
}

impl ScribeEventListener {
    /// Create a new listener that forwards events to `event_tx`.
    #[must_use]
    pub fn new(session_id: SessionId, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self { session_id, event_tx }
    }

    /// Session this listener belongs to.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Send a `MetadataEvent` on the channel, logging if the receiver has dropped.
    fn emit(&self, event: SessionEvent) {
        if self.event_tx.send(event).is_err() {
            debug!(
                session_id = %self.session_id,
                "SessionEvent dropped: receiver closed"
            );
        }
    }
}

impl EventListener for ScribeEventListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                self.emit(SessionEvent::Metadata(MetadataEvent::TitleChanged(title)));
            }

            Event::ResetTitle => {
                self.emit(SessionEvent::Metadata(MetadataEvent::TitleChanged(String::new())));
            }

            Event::Bell => {
                self.emit(SessionEvent::Metadata(MetadataEvent::Bell));
            }

            Event::ClipboardStore(kind, text) => {
                self.emit(SessionEvent::ClipboardStore(kind, text));
            }

            Event::ClipboardLoad(kind, formatter) => {
                self.emit(SessionEvent::ClipboardLoad(kind, formatter));
            }

            Event::PtyWrite(data) => {
                self.emit(SessionEvent::PtyWrite(data));
            }

            Event::ColorRequest(index, formatter) => {
                self.emit(SessionEvent::ColorRequest(index, formatter));
            }

            Event::TextAreaSizeRequest(formatter) => {
                self.emit(SessionEvent::TextAreaSizeRequest(formatter));
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
