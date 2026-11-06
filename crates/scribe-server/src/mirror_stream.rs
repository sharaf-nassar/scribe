//! Tab-mirror streams: a program rendering its own screen for a mirror.
//!
//! A tab mirror normally re-draws the pane's published grid. A program that
//! lays itself out for one terminal size cannot be shown larger that way, so
//! the Pi extension offers to render its screen a second time at the mirror's
//! size and streams those frames here through `scribe-hook-helper
//! --event=mirror_stream`, one long-lived connection per Pi session. This
//! module pairs that stream with the window mirroring the session: the
//! viewport the window asks for goes down to the stream, the frames the
//! stream renders go up to the window, and either side leaving tells the
//! other so the mirror falls back to its own rendering.

use std::collections::HashMap;
use std::sync::Arc;

use scribe_common::error::ScribeError;
use scribe_common::framing::read_message;
use scribe_common::ids::SessionId;
use scribe_common::protocol::{ClientMessage, ServerMessage, TerminalSize};
use tokio::sync::Mutex;

use crate::ipc_server::{IpcServerState, SharedWriter, send_message};

/// Every session with a stream attached, a mirror open, or both.
pub type MirrorStreams = Arc<Mutex<HashMap<SessionId, MirrorStream>>>;

/// One session's stream and the window mirroring it, either side optional.
#[derive(Default)]
pub struct MirrorStream {
    /// The helper connection rendering frames.
    stream: Option<SharedWriter>,
    /// The window's connection and the grid it asked for.
    viewer: Option<(SharedWriter, TerminalSize)>,
}

impl MirrorStream {
    const fn is_empty(&self) -> bool {
        self.stream.is_none() && self.viewer.is_none()
    }

    fn streamed_by(&self, writer: &SharedWriter) -> bool {
        self.stream.as_ref().is_some_and(|stream| Arc::ptr_eq(stream, writer))
    }

    fn viewed_by(&self, writer: &SharedWriter) -> bool {
        self.viewer.as_ref().is_some_and(|(viewer, _)| Arc::ptr_eq(viewer, writer))
    }

    /// The viewer to relay a frame from `stream` to, if `stream` is still the
    /// session's current one.
    fn relay_target(&self, stream: &SharedWriter) -> Option<SharedWriter> {
        self.streamed_by(stream)
            .then_some(self.viewer.as_ref())
            .flatten()
            .map(|(v, _)| Arc::clone(v))
    }

    /// The stop command for this entry's stream, when it has one.
    fn stop_for_stream(&self, session_id: SessionId) -> Option<(SharedWriter, ServerMessage)> {
        let size = TerminalSize::default();
        let msg = ServerMessage::MirrorViewport { session_id, size };
        self.stream.as_ref().map(|stream| (Arc::clone(stream), msg))
    }
}

/// Which sides of an entry a change should hear about.
struct Notify {
    stream: Option<(SharedWriter, ServerMessage)>,
    viewer: Option<(SharedWriter, ServerMessage)>,
}

impl Notify {
    async fn send(self) {
        for (writer, msg) in self.stream.into_iter().chain(self.viewer) {
            send_message(&writer, &msg).await;
        }
    }
}

/// Serve one `MirrorStreamAttach` connection until it closes.
///
/// Registers the stream for `session_id` (a newer stream for the same session
/// replaces an older one, which then relays nothing), hands it the viewport a
/// mirror already asked for, relays every frame it renders to that mirror, and
/// on exit tells the mirror the feed is gone.
pub async fn serve<R>(
    reader: &mut R,
    server: &IpcServerState,
    writer: &SharedWriter,
    session_id: SessionId,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    if !server.live_sessions.read().await.contains_key(&session_id) {
        tracing::debug!(%session_id, "mirror stream for an unknown session; closing");
        return;
    }
    let attached = {
        let mut streams = server.mirror_streams.lock().await;
        let entry = streams.entry(session_id).or_default();
        entry.stream = Some(Arc::clone(writer));
        Notify {
            stream: entry.viewer.as_ref().map(|(_, size)| {
                (Arc::clone(writer), ServerMessage::MirrorViewport { session_id, size: *size })
            }),
            viewer: entry.viewer.as_ref().map(|(viewer, _)| {
                (Arc::clone(viewer), ServerMessage::MirrorFeed { session_id, available: true })
            }),
        }
    };
    attached.send().await;
    tracing::info!(%session_id, "mirror stream attached");

    loop {
        match read_message::<ClientMessage, _>(reader).await {
            Ok(ClientMessage::MirrorFrame { session_id: frame_session, size, data })
                if frame_session == session_id =>
            {
                let streams = server.mirror_streams.lock().await;
                let viewer = streams.get(&session_id).and_then(|entry| entry.relay_target(writer));
                drop(streams);
                if let Some(viewer) = viewer {
                    send_message(&viewer, &ServerMessage::MirrorOutput { session_id, size, data })
                        .await;
                }
            }
            Ok(other) => tracing::debug!(?other, "ignored message on a mirror stream"),
            // The payload was consumed whole, so the next frame still aligns.
            Err(ScribeError::Deserialization { .. }) => {}
            Err(_) => break,
        }
    }

    let detached = {
        let mut streams = server.mirror_streams.lock().await;
        let Some(entry) = streams.get_mut(&session_id) else { return };
        if !entry.streamed_by(writer) {
            return;
        }
        entry.stream = None;
        let viewer = entry.viewer.as_ref().map(|(viewer, _)| {
            (Arc::clone(viewer), ServerMessage::MirrorFeed { session_id, available: false })
        });
        if entry.is_empty() {
            streams.remove(&session_id);
        }
        Notify { stream: None, viewer }
    };
    detached.send().await;
    tracing::info!(%session_id, "mirror stream closed");
}

/// A window opened, resized, closed, or scrolled its mirror of `session_id`.
/// The caller has already checked the window owns the session.
pub async fn handle_viewer(msg: ClientMessage, server: &IpcServerState, writer: &SharedWriter) {
    let notify = {
        let mut streams = server.mirror_streams.lock().await;
        match msg {
            ClientMessage::MirrorViewport { session_id, size } => {
                let entry = streams.entry(session_id).or_default();
                entry.viewer = Some((Arc::clone(writer), size));
                let available = entry.stream.is_some();
                Notify {
                    stream: entry.stream.as_ref().map(|stream| {
                        (Arc::clone(stream), ServerMessage::MirrorViewport { session_id, size })
                    }),
                    viewer: Some((
                        Arc::clone(writer),
                        ServerMessage::MirrorFeed { session_id, available },
                    )),
                }
            }
            ClientMessage::MirrorClose { session_id } => {
                let Some(entry) = streams.get_mut(&session_id) else { return };
                if !entry.viewed_by(writer) {
                    return;
                }
                entry.viewer = None;
                let stream = entry.stop_for_stream(session_id);
                if entry.is_empty() {
                    streams.remove(&session_id);
                }
                Notify { stream, viewer: None }
            }
            ClientMessage::MirrorScroll { session_id, delta } => {
                let Some(entry) = streams.get(&session_id) else { return };
                if !entry.viewed_by(writer) {
                    return;
                }
                Notify {
                    stream: entry.stream.as_ref().map(|stream| {
                        (Arc::clone(stream), ServerMessage::MirrorScroll { session_id, delta })
                    }),
                    viewer: None,
                }
            }
            other => {
                tracing::debug!(?other, "ignored non-mirror message in the mirror dispatcher");
                return;
            }
        }
    };
    notify.send().await;
}

/// A window's connection ended: close every mirror it was viewing, so their
/// streams stop rendering frames nobody receives.
pub async fn viewer_disconnected(server: &IpcServerState, writer: &SharedWriter) {
    let stops: Vec<(SharedWriter, ServerMessage)> = {
        let mut streams = server.mirror_streams.lock().await;
        let mut stops = Vec::new();
        streams.retain(|session_id, entry| {
            if entry.viewed_by(writer) {
                entry.viewer = None;
                stops.extend(entry.stop_for_stream(*session_id));
            }
            !entry.is_empty()
        });
        stops
    };
    for (stream, msg) in stops {
        send_message(&stream, &msg).await;
    }
}
