//! One-pane GPUI client spike over Scribe's frozen local IPC protocol.

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
    App, AsyncApp, Bounds, Context, Render, Task, WeakEntity, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
};
use gpui_platform::application;
use scribe_common::{
    framing::{read_message, write_message},
    protocol::{ClientMessage, ServerMessage, TerminalSize},
    screen_replay::{decompress_session_replay, snapshot_to_ansi},
    socket::server_socket_path,
};

use crate::{
    terminal::{Content, DisplayOnlyTerminal},
    terminal_element::TerminalElement,
};

const COLUMNS: u16 = 120;
const ROWS: u16 = 36;
const CELL_WIDTH: u16 = 8;
const CELL_HEIGHT: u16 = 18;

struct TerminalView {
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    status: Arc<Mutex<String>>,
    // Held to keep the redraw poll alive; dropping the view cancels the task.
    _refresh_task: Task<()>,
}

impl TerminalView {
    fn new(
        terminal: Arc<Mutex<DisplayOnlyTerminal>>,
        status: Arc<Mutex<String>>,
        generation: Arc<AtomicU64>,
        cx: &mut Context<Self>,
    ) -> Self {
        let refresh_task =
            cx.spawn(async move |view, app| drive_redraws(view, app, generation).await);
        Self { terminal, status, _refresh_task: refresh_task }
    }
}

/// Repaints the view whenever the IPC thread bumps the shared generation counter.
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let content =
            self.terminal.lock().map_or_else(|_| Content::default(), |guard| guard.content());
        let status = self
            .status
            .lock()
            .map_or_else(|_| "terminal state unavailable".to_owned(), |guard| guard.clone());

        div()
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
    let terminal =
        Arc::new(Mutex::new(DisplayOnlyTerminal::new(usize::from(COLUMNS), usize::from(ROWS))));
    let status = Arc::new(Mutex::new("connecting to Scribe server…".to_owned()));
    let generation = Arc::new(AtomicU64::new(0));
    let terminal_size = TerminalSize {
        cols: COLUMNS,
        rows: ROWS,
        cell_width: CELL_WIDTH,
        cell_height: CELL_HEIGHT,
    };
    start_ipc_thread(
        Arc::clone(&terminal),
        Arc::clone(&status),
        Arc::clone(&generation),
        terminal_size,
    );

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    TerminalView::new(
                        Arc::clone(&terminal),
                        Arc::clone(&status),
                        Arc::clone(&generation),
                        cx,
                    )
                })
            },
        ) {
            tracing::error!(%error, "failed to open GPUI window");
        }
        cx.activate(true);
    });
}

fn start_ipc_thread(
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    size: TerminalSize,
) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_io().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                set_status(&status, &generation, format!("IPC runtime failed: {error}"));
                return;
            }
        };
        runtime.block_on(async move {
            if let Err(error) =
                receive_one_pane(terminal, Arc::clone(&status), Arc::clone(&generation), size).await
            {
                set_status(&status, &generation, format!("server connection failed: {error}"));
            }
        });
    });
}

async fn receive_one_pane(
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    status: Arc<Mutex<String>>,
    generation: Arc<AtomicU64>,
    size: TerminalSize,
) -> Result<(), String> {
    let mut stream = tokio::net::UnixStream::connect(server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    write_message(
        &mut stream,
        &ClientMessage::Hello { window_id: None, clipboard_gating: false, takeover: false },
    )
    .await
    .map_err(|error| error.to_string())?;
    write_message(&mut stream, &ClientMessage::ListSessions)
        .await
        .map_err(|error| error.to_string())?;

    let mut attached_session = None;
    loop {
        let message: ServerMessage =
            read_message(&mut stream).await.map_err(|error| error.to_string())?;
        match message {
            ServerMessage::SessionList { sessions, .. } if attached_session.is_none() => {
                if let Some(session) = sessions.first() {
                    let session_id = session.session_id;
                    write_message(
                        &mut stream,
                        &ClientMessage::AttachSessions {
                            session_ids: vec![session_id],
                            dimensions: vec![size],
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    attached_session = Some(session_id);
                    set_status(&status, &generation, "attached to one live pane".to_owned());
                } else {
                    set_status(
                        &status,
                        &generation,
                        "connected; server has no live panes".to_owned(),
                    );
                }
            }
            ServerMessage::PtyOutput { session_id, data }
                if Some(session_id) == attached_session =>
            {
                write_terminal(&terminal, &generation, &data);
            }
            ServerMessage::SessionReplay { session_id, replay }
                if Some(session_id) == attached_session =>
            {
                let bytes =
                    decompress_session_replay(&replay).map_err(|error| error.to_string())?;
                write_terminal(&terminal, &generation, &bytes);
            }
            ServerMessage::ScreenSnapshot { session_id, snapshot }
                if Some(session_id) == attached_session =>
            {
                write_terminal(&terminal, &generation, &snapshot_to_ansi(&snapshot));
            }
            ServerMessage::Error { message } => set_status(&status, &generation, message),
            _ => {}
        }
    }
}

fn write_terminal(
    terminal: &Arc<Mutex<DisplayOnlyTerminal>>,
    generation: &AtomicU64,
    bytes: &[u8],
) {
    if let Ok(mut terminal) = terminal.lock() {
        terminal.write_output(bytes);
        generation.fetch_add(1, Ordering::Release);
    }
}

fn set_status(status: &Arc<Mutex<String>>, generation: &AtomicU64, message: String) {
    if let Ok(mut status) = status.lock() {
        *status = message;
        generation.fetch_add(1, Ordering::Release);
    }
}
