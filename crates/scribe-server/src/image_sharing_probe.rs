//! Bounded probe surface over the production attached-sink set.
//!
//! The terminal-image sharing gate has to certify viewer counting, controller
//! changes, and detach against the real [`AttachedSinks`] rather than a
//! re-implementation, so the exact production types are reachable here. No
//! server path calls anything in this module.

use crate::ipc_server::{
    AttachedSinks, ClientSink, ClientWriter, OutputSink, SessionImageState, SharedImageSharing,
    SharedWriter, begin_sink_attach, drain_image_replay_debt, finish_sink_attach,
    image_replay_debt, lock_sinks, new_live_session_registry, send_image_records,
    send_image_replay, spawn_output_queue,
};
use scribe_common::ids::SessionId;
use scribe_common::protocol::ServerMessage;
use scribe_common::terminal_images::TerminalImageCapabilities;
use std::sync::Arc;
use tokio::sync::Mutex;

/// One synthetic viewer connection: a real bounded output queue, its drain
/// task, and the pipe the gate reads delivered frames back out of.
pub struct ProbeViewer {
    /// Attach/detach identity token, exactly as a real connection's.
    pub writer: SharedWriter,
    sink: OutputSink,
    reader: tokio::io::DuplexStream,
}

impl ProbeViewer {
    /// Read back every frame delivered so far, stopping when the pipe is
    /// momentarily empty. Bounded by the queue that produced them.
    pub async fn drain(&mut self) -> Vec<ServerMessage> {
        let mut frames = Vec::new();
        while let Ok(Ok(message)) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            scribe_common::framing::read_message::<ServerMessage, _>(&mut self.reader),
        )
        .await
        {
            frames.push(message);
        }
        frames
    }

    /// This viewer's advertised renderer capability.
    pub fn capabilities(&self) -> TerminalImageCapabilities {
        self.sink.image_capabilities()
    }
}

/// An empty per-session sink set.
#[must_use]
pub fn new_client_writer() -> ClientWriter {
    Arc::new(std::sync::Mutex::new(AttachedSinks::default()))
}

/// Attach one viewer with the given capability. `additive` mirrors the
/// production split: a shared-mode join keeps the other participants'
/// sinks, a `SingleController` re-point replaces the whole set.
pub async fn attach_viewer(
    client_writer: &ClientWriter,
    capabilities: TerminalImageCapabilities,
    additive: bool,
) -> ProbeViewer {
    let viewer = begin_attach_viewer(client_writer, capabilities, additive).await;
    finish_sink_attach(client_writer, &viewer.writer, 0, SessionId::new());
    viewer
}

/// Install a viewer that is still buffering — the production window between a
/// sink's install and its replay landing on the wire.
pub async fn begin_attach_viewer(
    client_writer: &ClientWriter,
    capabilities: TerminalImageCapabilities,
    additive: bool,
) -> ProbeViewer {
    let (reader, write_half) = tokio::io::duplex(1 << 20);
    let (sink, _drain) = spawn_output_queue(write_half, new_live_session_registry());
    sink.set_image_capabilities(capabilities);
    let writer: SharedWriter = Arc::new(Mutex::new(ClientSink::new(sink.clone())));
    begin_sink_attach(client_writer, &writer, additive).await;
    ProbeViewer { writer, sink, reader }
}

/// Detach one viewer through the production identity guard.
pub fn detach_viewer(client_writer: &ClientWriter, viewer: &ProbeViewer) -> bool {
    lock_sinks(client_writer).detach(&viewer.writer)
}

/// Fan one live burst out through the production capable-sink path.
pub fn fan_out_images(
    client_writer: &ClientWriter,
    session_id: SessionId,
    required: TerminalImageCapabilities,
    messages: &[ServerMessage],
) -> usize {
    send_image_records(client_writer, session_id, required, messages)
}

/// How many capable sinks currently owe a combined image replay.
pub fn replay_debt(client_writer: &ClientWriter, required: TerminalImageCapabilities) -> usize {
    image_replay_debt(client_writer, required)
}

/// Fan one planned replay burst out through the production recovery path.
pub fn fan_out_image_replay(
    client_writer: &ClientWriter,
    required: TerminalImageCapabilities,
    records: &[ServerMessage],
) -> usize {
    send_image_replay(client_writer, required, records)
}

/// Drain replay debt the way the production attach path does, straight from
/// canonical session state and without a PTY read to ride on.
pub async fn drain_attach_replay_debt(
    client_writer: &ClientWriter,
    session_id: SessionId,
    images: &SessionImageState,
    sharing: &SharedImageSharing,
) {
    drain_image_replay_debt(client_writer, session_id, images, sharing).await;
}

/// Release a viewer's replay debt the way the production attach path does.
pub fn finish_attach(client_writer: &ClientWriter, viewer: &ProbeViewer, session_id: SessionId) {
    finish_sink_attach(client_writer, &viewer.writer, 0, session_id);
}
