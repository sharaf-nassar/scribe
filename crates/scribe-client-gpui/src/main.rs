//! One-pane GPUI client spike over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod terminal;
mod terminal_element;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use gpui::{
    App, AsyncApp, Bounds, Context, FocusHandle, KeyDownEvent, Render, Task, TitlebarOptions,
    WeakEntity, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;
use scribe_common::{
    framing::{read_message, write_message},
    ids::SessionId,
    protocol::{ClientMessage, ServerMessage, SessionInfo, TerminalSize},
    screen_replay::{decompress_session_replay, snapshot_to_ansi},
    socket::server_socket_path,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::{
    ipc_bridge::{InboundEvent, IpcSink, run_drain},
    terminal::{Content, DisplayOnlyTerminal},
    terminal_element::TerminalElement,
};

const COLUMNS: u16 = 120;
const ROWS: u16 = 36;
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 18;

/// Shared handles threaded from the app entry into the background IPC thread and
/// the foreground GPUI view.
#[derive(Clone)]
struct Shared {
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    active_session: Arc<Mutex<Option<SessionId>>>,
}

struct TerminalView {
    shared: Shared,
    sink: IpcSink,
    focus_handle: FocusHandle,
    // Held to keep the redraw poll alive; dropping the view cancels the task.
    _refresh_task: Task<()>,
}

impl TerminalView {
    fn new(shared: Shared, sink: IpcSink, cx: &mut Context<Self>) -> Self {
        let generation = Arc::clone(&shared.generation);
        let refresh_task =
            cx.spawn(async move |view, app| drive_redraws(view, app, generation).await);
        Self { shared, sink, focus_handle: cx.focus_handle(), _refresh_task: refresh_task }
    }

    /// Encodes a keystroke and enqueues it as `KeyInput` for the attached pane.
    ///
    /// Interim passthrough encoder: printable characters plus a handful of
    /// control keys. The full kitty/CSI-u encoder lands with the input-encoder
    /// port; this only proves the outbound [`IpcSink`] path end to end.
    fn on_key_down(&self, event: &KeyDownEvent) {
        let Some(bytes) = encode_key(event) else {
            return;
        };
        let session_id = self.shared.active_session.lock().ok().and_then(|guard| *guard);
        let Some(session_id) = session_id else {
            return;
        };
        if let Err(error) = self.sink.key_input(session_id, bytes, true) {
            tracing::warn!(%error, "dropped keystroke: IPC writer closed");
        }
    }
}

/// Interim keystroke encoder feeding the outbound [`IpcSink`] (see
/// [`TerminalView::on_key_down`]).
fn encode_key(event: &KeyDownEvent) -> Option<Vec<u8>> {
    if let Some(text) = event.keystroke.key_char.as_ref()
        && !text.is_empty()
    {
        return Some(text.clone().into_bytes());
    }
    let bytes: &[u8] = match event.keystroke.key.as_str() {
        "enter" => b"\r",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "escape" => b"\x1b",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        _ => return None,
    };
    Some(bytes.to_vec())
}

/// Repaints the view whenever the IPC drain bumps the shared generation counter.
async fn drive_redraws(
    view: WeakEntity<TerminalView>,
    app: &mut AsyncApp,
    generation: Arc<AtomicU64>,
) {
    let mut rendered = generation.load(Ordering::Acquire);
    loop {
        app.background_executor().timer(Duration::from_millis(16)).await;
        let current = generation.load(Ordering::Acquire);
        if current == rendered {
            continue;
        }
        rendered = current;
        if view.update(app, |_, view_cx| view_cx.notify()).is_err() {
            return;
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        let content = self
            .shared
            .terminal
            .lock()
            .map_or_else(|_| Content::default(), |guard| guard.content());
        let status = self
            .shared
            .status
            .lock()
            .map_or_else(|_| "terminal state unavailable".to_owned(), |guard| guard.clone());

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, _cx| {
                view.on_key_down(event);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0010_1318))
            .child(TerminalElement::new(content).paint())
            .child(
                div()
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(rgb(0x001b_2230))
                    .text_color(rgb(0x009f_b0c5))
                    .text_xs()
                    .child(status),
            )
    }
}

fn main() {
    let shared = Shared {
        terminal: Arc::new(Mutex::new(DisplayOnlyTerminal::new(
            usize::from(COLUMNS),
            usize::from(ROWS),
        ))),
        status: Arc::new(Mutex::new("connecting to Scribe server…".to_owned())),
        generation: Arc::new(AtomicU64::new(0)),
        active_session: Arc::new(Mutex::new(None)),
    };
    let terminal_size = TerminalSize {
        cols: COLUMNS,
        rows: ROWS,
        cell_width: CELL_WIDTH,
        cell_height: CELL_HEIGHT,
    };
    let (out_tx, out_rx) = unbounded_channel::<ClientMessage>();
    let (in_tx, in_rx) = unbounded_channel::<InboundEvent>();
    let sink = IpcSink::new(out_tx.clone());

    start_ipc_thread(IpcThread {
        shared: shared.clone(),
        sink: sink.clone(),
        out_tx,
        out_rx,
        in_tx,
        in_rx,
        size: terminal_size,
    });

    application().run(move |cx: &mut App| {
        open_window(cx, &shared, &sink);
        cx.activate(true);
    });
}

fn open_window(cx: &mut App, shared: &Shared, sink: &IpcSink) {
    let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
    let shared = shared.clone();
    let sink = sink.clone();
    if let Err(error) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // Set WM_NAME/_NET_WM_NAME to "Scribe" so the X11 visual-E2E harness
            // (`docker/entrypoint-visual.sh`) can locate the window with
            // `xdotool search --name "Scribe"` for focus and screenshot capture.
            titlebar: Some(TitlebarOptions { title: Some("Scribe".into()), ..Default::default() }),
            app_id: Some("scribe".to_owned()),
            ..Default::default()
        },
        |_, cx| cx.new(|cx| TerminalView::new(shared, sink, cx)),
    ) {
        tracing::error!(%error, "failed to open GPUI window");
    }
}

/// Everything the background IPC thread owns for one connection.
struct IpcThread {
    shared: Shared,
    sink: IpcSink,
    out_tx: UnboundedSender<ClientMessage>,
    out_rx: UnboundedReceiver<ClientMessage>,
    in_tx: UnboundedSender<InboundEvent>,
    in_rx: UnboundedReceiver<InboundEvent>,
    size: TerminalSize,
}

fn start_ipc_thread(ctx: IpcThread) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                set_status(
                    &ctx.shared.status,
                    &ctx.shared.generation,
                    format!("IPC runtime failed: {error}"),
                );
                return;
            }
        };
        runtime.block_on(async move {
            let status = Arc::clone(&ctx.shared.status);
            let generation = Arc::clone(&ctx.shared.generation);
            if let Err(error) = run_connection(ctx).await {
                set_status(&status, &generation, format!("server connection failed: {error}"));
            }
        });
    });
}

async fn run_connection(ctx: IpcThread) -> Result<(), String> {
    let stream = tokio::net::UnixStream::connect(server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    let (reader, writer) = stream.into_split();

    // Handshake is queued ahead of any sink traffic on the same ordered channel.
    ctx.out_tx
        .send(ClientMessage::Hello { window_id: None, clipboard_gating: false, takeover: false })
        .map_err(|_| "writer channel closed".to_owned())?;
    ctx.out_tx.send(ClientMessage::ListSessions).map_err(|_| "writer channel closed".to_owned())?;

    tokio::spawn(run_writer(writer, ctx.out_rx));
    spawn_drain(ctx.in_rx, Arc::clone(&ctx.shared.terminal), Arc::clone(&ctx.shared.generation));

    run_reader(
        reader,
        ReaderCtx {
            status: ctx.shared.status,
            generation: ctx.shared.generation,
            active_session: ctx.shared.active_session,
            out_tx: ctx.out_tx,
            in_tx: ctx.in_tx,
            sink: ctx.sink,
            size: ctx.size,
        },
    )
    .await
}

/// Drains the IPC-writer channel to the socket in FIFO order.
async fn run_writer<W>(mut writer: W, mut out_rx: UnboundedReceiver<ClientMessage>)
where
    W: AsyncWriteExt + Unpin,
{
    while let Some(message) = out_rx.recv().await {
        if let Err(error) = write_message(&mut writer, &message).await {
            tracing::warn!(%error, "IPC writer task stopped");
            return;
        }
    }
}

/// Spawns the coalescing drain: batched `write_output` + one repaint per dirty
/// pane, mirroring Zed's terminal wakeup coalescing.
fn spawn_drain(
    in_rx: UnboundedReceiver<InboundEvent>,
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    generation: Arc<AtomicU64>,
) {
    tokio::spawn(run_drain(in_rx, move |batch| {
        if batch.is_empty() {
            return;
        }
        let dirty = batch.len();
        if let Ok(mut guard) = terminal.lock() {
            for (_session, bytes) in batch.iter() {
                guard.write_output(bytes);
            }
        }
        for _ in 0..dirty {
            generation.fetch_add(1, Ordering::Release);
        }
    }));
}

/// Handles owned by the inbound read loop.
struct ReaderCtx {
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    active_session: Arc<Mutex<Option<SessionId>>>,
    out_tx: UnboundedSender<ClientMessage>,
    in_tx: UnboundedSender<InboundEvent>,
    sink: IpcSink,
    size: TerminalSize,
}

async fn run_reader<R>(mut reader: R, ctx: ReaderCtx) -> Result<(), String>
where
    R: AsyncReadExt + Unpin,
{
    let mut attached: Option<SessionId> = None;
    loop {
        let message: ServerMessage =
            read_message(&mut reader).await.map_err(|error| error.to_string())?;
        match message {
            ServerMessage::SessionList { sessions, .. } if attached.is_none() => {
                attach_first(&ctx, &sessions, &mut attached)?;
            }
            ServerMessage::PtyOutput { session_id, data } if Some(session_id) == attached => {
                forward_output(&ctx.in_tx, session_id, data);
            }
            ServerMessage::SessionReplay { session_id, replay } if Some(session_id) == attached => {
                let bytes =
                    decompress_session_replay(&replay).map_err(|error| error.to_string())?;
                forward_output(&ctx.in_tx, session_id, bytes);
            }
            ServerMessage::ScreenSnapshot { session_id, snapshot }
                if Some(session_id) == attached =>
            {
                forward_output(&ctx.in_tx, session_id, snapshot_to_ansi(&snapshot));
            }
            ServerMessage::Error { message } => set_status(&ctx.status, &ctx.generation, message),
            _ => {}
        }
    }
}

fn attach_first(
    ctx: &ReaderCtx,
    sessions: &[SessionInfo],
    attached: &mut Option<SessionId>,
) -> Result<(), String> {
    let Some(session) = sessions.first() else {
        set_status(&ctx.status, &ctx.generation, "connected; server has no live panes".to_owned());
        return Ok(());
    };
    let session_id = session.session_id;
    ctx.out_tx
        .send(ClientMessage::AttachSessions {
            session_ids: vec![session_id],
            dimensions: vec![ctx.size],
        })
        .map_err(|_| "writer channel closed".to_owned())?;
    // Announce the client size through the sink, ahead of any KeyInput.
    ctx.sink.resize(session_id, ctx.size).map_err(|error| error.to_string())?;
    *attached = Some(session_id);
    if let Ok(mut guard) = ctx.active_session.lock() {
        *guard = Some(session_id);
    }
    set_status(&ctx.status, &ctx.generation, "attached to one live pane".to_owned());
    Ok(())
}

fn forward_output(in_tx: &UnboundedSender<InboundEvent>, session_id: SessionId, bytes: Vec<u8>) {
    if in_tx.send(InboundEvent::PaneOutput { session_id, bytes }).is_err() {
        tracing::warn!("inbound drain closed; dropping pane output");
    }
}

fn set_status(status: &Arc<Mutex<String>>, generation: &AtomicU64, message: String) {
    if let Ok(mut status) = status.lock() {
        *status = message;
        generation.fetch_add(1, Ordering::Release);
    }
}
