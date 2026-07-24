//! One-pane GPUI client spike over Scribe's frozen local IPC protocol.

mod ipc_bridge;
mod session_lifecycle;
mod sync_frames;
mod terminal;
mod terminal_element;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    Point, Render, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;
use scribe_client_gpui::animation::AnimationSettings;
use scribe_client_gpui::command_palette::{
    CommandPaletteColors, CommandPaletteEvent, CommandPaletteView, build_entries,
};
use scribe_client_gpui::context_menu::{
    ContextMenuColors, ContextMenuEvent, ContextMenuRequest, ContextMenuView,
};
use scribe_client_gpui::dialog::{
    AnyDialog, ClipboardDialog, CloseDialog, DialogColors, DialogEvent, DialogView,
};
use scribe_client_gpui::layout::Rect;
use scribe_client_gpui::status_bar::{self, RemoteStatusData, StatusBarColors, StatusBarData};
use scribe_client_gpui::sys_stats::SystemStatsCollector;
use scribe_client_gpui::tooltip::{TooltipColors, TooltipPosition, TooltipRender, tooltip_element};
use scribe_client_gpui::{
    tab_bar::{TabBarColors, TabData},
    titlebar::TitlebarView,
};
use scribe_common::theme::ChromeColors;
use scribe_common::{
    config::{StatusBarStatsConfig, load_config, resolve_theme},
    framing::{read_message, write_message},
    ids::SessionId,
    protocol::{ClientMessage, ServerMessage, SessionInfo, TerminalSize},
    socket::server_socket_path,
    theme::minimal_dark,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{
        Notify,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
};

use crate::{
    ipc_bridge::{InboundEvent, IpcSink, run_drain},
    sync_frames::{SyncFrameQueue, drain_all_committed},
    terminal::{Content, DisplayOnlyTerminal},
    terminal_element::TerminalElement,
};

/// Wall-clock origin captured at the very top of `main`, used to time
/// startup-to-first-frame for the perf A/B rig (`tools/perf-ab-rig`).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// Latches once the first frame has emitted its startup-timing marker so the
/// per-frame `render` hook only measures the initial paint.
static FIRST_FRAME_LOGGED: AtomicBool = AtomicBool::new(false);

/// Env var that opts the client into the perf-rig startup marker. Its value is
/// a file path; when set to a non-empty path, the first `render` writes a
/// machine-parseable `first_frame_ms=<n>` marker to that file. The rig reads it
/// and gates it against the 500 ms Clarification-Q3 budget. Unset by default so
/// normal runs write nothing.
const STARTUP_TIMING_ENV: &str = "SCRIBE_GPUI_STARTUP_TIMING";

/// Writes the startup-to-first-frame marker exactly once, on the first painted
/// frame, when [`STARTUP_TIMING_ENV`] names an output file. The elapsed time is
/// measured from [`PROCESS_START`] so it captures the full window from process
/// launch through GPU-ready first paint, mirroring the old client's
/// `init_gpu_and_terminal_done` method that produced the recorded baseline.
fn log_first_frame_timing() {
    if FIRST_FRAME_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(path) = std::env::var_os(STARTUP_TIMING_ENV).filter(|value| !value.is_empty()) else {
        return;
    };
    let Some(start) = PROCESS_START.get() else {
        return;
    };
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    if let Err(error) = std::fs::write(&path, format!("first_frame_ms={elapsed_ms:.3}\n")) {
        tracing::warn!(%error, "failed to write startup-timing marker");
    }
}

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
    /// Server-connection flag driving the status bar's connection dot.
    connected: Arc<AtomicBool>,
}

struct TerminalView {
    shared: Shared,
    sink: IpcSink,
    focus_handle: FocusHandle,
    /// System-stats sampler feeding the status bar's CPU/MEM/NET/GPU sparklines.
    stats: SystemStatsCollector,
    /// Theme-derived status-bar palette, resolved once at view creation.
    status_colors: StatusBarColors,
    /// Which system-stat segments the status bar shows, from config.
    stats_config: StatusBarStatsConfig,
    // The custom titlebar + integrated tab bar drawn above the terminal grid.
    titlebar: Entity<TitlebarView>,
    /// Theme chrome, retained to build the overlay palettes on demand.
    chrome: ChromeColors,
    /// The command palette overlay, present only while open.
    command_palette: Option<Entity<CommandPaletteView>>,
    /// The right-click context menu overlay, present only while open.
    context_menu: Option<Entity<ContextMenuView>>,
    /// The modal dialog overlay, present only while a modal is open. The spike
    /// wires two representative dialogs (close + clipboard) so the visual E2E
    /// can screenshot the ported modal chrome and its focus/button behaviour.
    dialog: Option<Entity<DialogView>>,
    /// Demo toggle: when set, an OSC 8-style hover tooltip is drawn over a fixed
    /// anchor so the visual E2E can exercise tooltip clamping + URL truncation.
    tooltip_demo: bool,
    // Held to keep the redraw poll alive; dropping the view cancels the task.
    _refresh_task: Task<()>,
}

impl TerminalView {
    fn new(shared: Shared, sink: IpcSink, cx: &mut Context<Self>) -> Self {
        let generation = Arc::clone(&shared.generation);
        let refresh_task =
            cx.spawn(async move |view, app| drive_redraws(view, app, generation).await);
        let config = load_config().unwrap_or_default();
        let theme = resolve_theme(&config);
        let status_colors = StatusBarColors::from_theme(&theme.chrome, &theme.ansi_colors);
        let chrome = theme.chrome;
        let colors = TabBarColors::from(&minimal_dark().chrome);
        let titlebar = cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, cx);
            let mut tab = TabData::new("shell");
            tab.is_active = true;
            bar.set_tabs(vec![tab], cx);
            bar
        });
        Self {
            shared,
            sink,
            focus_handle: cx.focus_handle(),
            stats: SystemStatsCollector::new(),
            status_colors,
            stats_config: config.terminal.status_bar_stats,
            titlebar,
            chrome,
            command_palette: None,
            context_menu: None,
            dialog: None,
            tooltip_demo: false,
            _refresh_task: refresh_task,
        }
    }

    /// Open the command palette, building its entry list from the live update /
    /// profile state, and subscribe to its confirm/dismiss events so a choice or
    /// an outside click tears the overlay down.
    fn open_command_palette(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        let profile_names = scribe_common::profiles::list_profiles().unwrap_or_default();
        let active = scribe_common::profiles::active_profile_name().ok();
        let entries = build_entries(None, &profile_names, active.as_deref());
        let colors = CommandPaletteColors::from(&self.chrome);
        let palette = cx.new(|cx| CommandPaletteView::new(colors, entries, cx));
        cx.subscribe(&palette, |this, _palette, event: &CommandPaletteEvent, ctx| match event {
            CommandPaletteEvent::Execute(_) | CommandPaletteEvent::Dismissed => {
                this.command_palette = None;
                ctx.notify();
            }
        })
        .detach();
        self.command_palette = Some(palette);
        cx.notify();
    }

    /// Open the right-click context menu at `position` with a representative item
    /// set (selection + OSC 8 URL), subscribing so a choice or dismiss closes it.
    fn open_context_menu(&mut self, position: Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.command_palette = None;
        let colors = ContextMenuColors::from(&self.chrome);
        let request = ContextMenuRequest {
            has_selection: true,
            osc8_uri: Some("https://example.com/spec".into()),
            ..Default::default()
        };
        let menu = cx.new(|cx| ContextMenuView::new(colors, request, position, cx));
        cx.subscribe(&menu, |this, _menu, event: &ContextMenuEvent, ctx| match event {
            ContextMenuEvent::Selected(_) | ContextMenuEvent::Dismissed => {
                this.context_menu = None;
                ctx.notify();
            }
        })
        .detach();
        self.context_menu = Some(menu);
        cx.notify();
    }

    /// Open a modal dialog, subscribing so a choice or a backdrop click tears the
    /// overlay down. The other overlays are dismissed so only one modal is up.
    fn open_dialog(&mut self, dialog: AnyDialog, cx: &mut Context<Self>) {
        self.command_palette = None;
        self.context_menu = None;
        let colors = DialogColors::from(&self.chrome);
        let view = cx.new(|cx| DialogView::new(dialog, colors, cx));
        cx.subscribe(&view, |this, _view, event: &DialogEvent, ctx| match event {
            DialogEvent::Chosen(_) => {
                this.dialog = None;
                ctx.notify();
            }
        })
        .detach();
        self.dialog = Some(view);
        cx.notify();
    }

    /// Route a keystroke while an overlay owns the keyboard. Returns `true` when
    /// the key was consumed by an overlay (and must not reach the PTY).
    fn handle_overlay_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let mods = &event.keystroke.modifiers;
        // Ctrl+Shift+P opens the palette; Ctrl+Shift+U toggles the tooltip demo;
        // Ctrl+Shift+Q opens the close dialog; Ctrl+Shift+K opens the clipboard
        // dialog (the two representative modals the visual E2E screenshots).
        if mods.control && mods.shift && self.dialog.is_none() {
            match event.keystroke.key.as_str() {
                "p" => {
                    self.open_command_palette(cx);
                    return true;
                }
                "u" => {
                    self.tooltip_demo = !self.tooltip_demo;
                    cx.notify();
                    return true;
                }
                "q" => {
                    self.open_dialog(AnyDialog::Close(CloseDialog::new(1)), cx);
                    return true;
                }
                "k" => {
                    self.open_dialog(
                        AnyDialog::Clipboard(ClipboardDialog::new(
                            scribe_common::protocol::PromptId(1),
                            scribe_common::protocol::ClipboardOp::Write,
                            scribe_common::protocol::ClipboardSelection::Clipboard,
                            Some("export TOKEN=hunter2".to_owned()),
                        )),
                        cx,
                    );
                    return true;
                }
                _ => {}
            }
        }

        // A modal dialog owns the keyboard while it is up: Tab/Shift+Tab cycle
        // focus, Enter activates the focused button, Esc dismisses (safe action).
        if let Some(dialog) = self.dialog.clone() {
            match event.keystroke.key.as_str() {
                "escape" => dialog.update(cx, DialogView::dismiss),
                "enter" => dialog.update(cx, DialogView::confirm),
                "tab" if mods.shift => dialog.update(cx, DialogView::focus_prev),
                "tab" | "right" => dialog.update(cx, DialogView::focus_next),
                "left" => dialog.update(cx, DialogView::focus_prev),
                _ => {}
            }
            return true;
        }

        let Some(palette) = self.command_palette.clone() else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => palette.update(cx, CommandPaletteView::dismiss),
            "enter" => palette.update(cx, CommandPaletteView::confirm),
            "backspace" => palette.update(cx, CommandPaletteView::pop_char),
            "up" => palette.update(cx, CommandPaletteView::prev_item),
            "down" => palette.update(cx, CommandPaletteView::next_item),
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref().filter(|t| !t.is_empty()) {
                    let text = text.clone();
                    palette.update(cx, |p, ctx| p.push_str(&text, ctx));
                }
            }
        }
        true
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

impl TerminalView {
    /// Build the status-bar segment model from the live connection / stats state.
    fn build_status_model(&mut self) -> status_bar::StatusBarModel {
        let connected = self.shared.connected.load(Ordering::Acquire);
        let session_count =
            usize::from(self.shared.active_session.lock().ok().and_then(|guard| *guard).is_some());
        // Refresh the sparkline sampler (internally rate-limited to 2 s) and
        // build the full segment model from the live data available so far.
        let sys_stats = self.stats.maybe_refresh();
        status_bar::build_model(
            &StatusBarData {
                connected,
                workspace_name: None,
                cwd: None,
                git_branch: None,
                last_command_status: None,
                env_status: None,
                session_count,
                remote: RemoteStatusData { enabled: false, controllers: &[] },
                share_presence: None,
                host_label: "local",
                remote_transport: None,
                tmux_label: None,
                time: "",
                update_available: None,
                update_progress: None,
                sys_stats: Some(sys_stats),
                stats_config: Some(&self.stats_config),
            },
            &self.status_colors,
        )
    }

    /// Build the demo hover tooltip when the `tooltip_demo` toggle is on: a long
    /// URI anchored near the right edge, exercising both the head+tail truncation
    /// and the viewport clamp.
    fn build_tooltip_demo(&self) -> Option<gpui::AnyElement> {
        self.tooltip_demo.then(|| {
            let colors = TooltipColors::from(&self.chrome);
            let anchor = Rect { x: 780.0, y: 120.0, width: 120.0, height: 18.0 };
            let display = scribe_client_gpui::tooltip::truncate_url(
                "https://example.com/very/long/path/that/overflows/the/box",
                48,
            );
            tooltip_element(&TooltipRender {
                text: &display,
                anchor,
                position: TooltipPosition::Below,
                viewport_width: f32::from(CELL_WIDTH) * f32::from(COLUMNS),
                colors: &colors,
                char_width: f32::from(CELL_WIDTH),
                line_height: f32::from(CELL_HEIGHT),
            })
        })
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        log_first_frame_timing();
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

        let model = self.build_status_model();
        let tooltip = self.build_tooltip_demo();

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _window, ctx| {
                if !view.handle_overlay_key(event, ctx) {
                    view.on_key_down(event);
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x0010_1318))
            .child(self.titlebar.clone())
            .child(
                // Wrap the terminal grid so a right-click opens the context menu
                // at the cursor without disturbing the display-only element.
                div().flex_1().child(TerminalElement::new(content).paint()).on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|view, event: &MouseDownEvent, _window, ctx| {
                        view.open_context_menu(event.position, ctx);
                    }),
                ),
            )
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
            .child(status_bar::render(&model, 24., &self.status_colors))
            .children(self.command_palette.clone())
            .children(self.context_menu.clone())
            .children(self.dialog.clone())
            .children(tooltip)
    }
}

fn main() {
    PROCESS_START.get_or_init(Instant::now);
    let shared = Shared {
        terminal: Arc::new(Mutex::new(DisplayOnlyTerminal::new(
            usize::from(COLUMNS),
            usize::from(ROWS),
        ))),
        status: Arc::new(Mutex::new("connecting to Scribe server…".to_owned())),
        generation: Arc::new(AtomicU64::new(0)),
        active_session: Arc::new(Mutex::new(None)),
        connected: Arc::new(AtomicBool::new(false)),
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
        // Resolve the motion policy from `appearance.animations` (default true)
        // and the SCRIBE_DISABLE_ANIMATIONS override, then mirror it onto GPUI's
        // global reduce-motion flag so any UI transitions stay off — and
        // screenshots stay byte-identical — under the E2E determinism path.
        let animations = load_config().map_or(true, |config| config.appearance.animations);
        AnimationSettings::resolve(animations).apply_to_app(cx);
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
            let connected = Arc::clone(&ctx.shared.connected);
            if let Err(error) = run_connection(ctx).await {
                connected.store(false, Ordering::Release);
                set_status(&status, &generation, format!("server connection failed: {error}"));
            }
        });
    });
}

async fn run_connection(ctx: IpcThread) -> Result<(), String> {
    let stream = tokio::net::UnixStream::connect(server_socket_path())
        .await
        .map_err(|error| error.to_string())?;
    // Socket is up: light the status-bar connection dot green.
    ctx.shared.connected.store(true, Ordering::Release);
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

/// Per-session synchronized-output frame queues shared between the coalescing
/// drain (which enqueues committed bursts) and the expiry flusher.
type SyncFrameQueues = Arc<Mutex<HashMap<SessionId, SyncFrameQueue>>>;

/// Spawns the coalescing drain with synchronized-frame queueing in front of
/// `write_output`. A first task drains the inbound channel with Zed's
/// 4 ms / 100-event coalescing ([`run_drain`]), splits each pane's bytes into
/// committed `CSI ? 2026` bursts via a per-session [`SyncFrameQueue`], and
/// replays one burst per redraw so no frame tears across IPC message
/// boundaries. A second task waits on the nearest raw-frame or parser sync
/// deadline and flushes a 150 ms-expired update whose terminating `CSI ? 2026 l`
/// never arrived.
fn spawn_drain(
    in_rx: UnboundedReceiver<InboundEvent>,
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    generation: Arc<AtomicU64>,
) {
    let queues: SyncFrameQueues = Arc::new(Mutex::new(HashMap::new()));
    let expiry_wake = Arc::new(Notify::new());

    let queue_task = Arc::clone(&queues);
    let terminal_task = Arc::clone(&terminal);
    let generation_task = Arc::clone(&generation);
    let wake_task = Arc::clone(&expiry_wake);
    tokio::spawn(run_drain(in_rx, move |batch| {
        if batch.is_empty() {
            return;
        }
        let mut redraws = 0usize;
        let mut sync_armed = false;
        if let (Ok(mut session_queues), Ok(mut guard)) = (queue_task.lock(), terminal_task.lock()) {
            for (session, bytes) in batch.iter() {
                let queue = session_queues.entry(session).or_default();
                queue.queue_output_frames(bytes);
                redraws += usize::from(drain_all_committed(queue, &mut *guard).needs_redraw);
                sync_armed |= queue.raw_sync_deadline().is_some();
            }
            sync_armed |= guard.parser_sync_deadline().is_some();
        }
        for _ in 0..redraws {
            generation_task.fetch_add(1, Ordering::Release);
        }
        if sync_armed {
            wake_task.notify_one();
        }
    }));

    tokio::spawn(run_sync_expiry(queues, terminal, generation, expiry_wake));
}

/// Waits on the nearest synchronized-update deadline and commits it once it
/// expires, so an unterminated `CSI ? 2026 h` still flushes after 150 ms even
/// while the inbound channel is idle. When nothing is buffering, it parks until
/// the drain task arms a fresh deadline.
async fn run_sync_expiry(
    queues: SyncFrameQueues,
    terminal: Arc<Mutex<DisplayOnlyTerminal>>,
    generation: Arc<AtomicU64>,
    wake: Arc<Notify>,
) {
    loop {
        match next_sync_deadline(&queues, &terminal) {
            None => wake.notified().await,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    () = tokio::time::sleep(remaining) => flush_expired_sync(&queues, &terminal, &generation),
                    () = wake.notified() => {}
                }
            }
        }
    }
}

/// Commits every raw-frame and parser synchronized update whose deadline has
/// passed, bumping the redraw generation once per committed burst.
fn flush_expired_sync(
    queues: &SyncFrameQueues,
    terminal: &Arc<Mutex<DisplayOnlyTerminal>>,
    generation: &Arc<AtomicU64>,
) {
    let now = Instant::now();
    let mut redraws = 0usize;
    if let (Ok(mut session_queues), Ok(mut guard)) = (queues.lock(), terminal.lock()) {
        redraws += usize::from(guard.flush_parser_sync_timeout(now));
        for queue in session_queues.values_mut() {
            let flushed = queue.flush_raw_timeout(now)
                && drain_all_committed(queue, &mut *guard).needs_redraw;
            redraws += usize::from(flushed);
        }
    }
    for _ in 0..redraws {
        generation.fetch_add(1, Ordering::Release);
    }
}

/// Nearest synchronized-update deadline across every pane's raw-frame queue and
/// the shared parser, or `None` when nothing is buffering.
fn next_sync_deadline(
    queues: &SyncFrameQueues,
    terminal: &Arc<Mutex<DisplayOnlyTerminal>>,
) -> Option<Instant> {
    let parser = terminal.lock().ok().and_then(|guard| guard.parser_sync_deadline());
    let raw = queues
        .lock()
        .ok()
        .and_then(|queues| queues.values().filter_map(SyncFrameQueue::raw_sync_deadline).min());
    match (parser, raw) {
        (Some(parser), Some(raw)) => Some(parser.min(raw)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
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
    let mut registry = session_lifecycle::SessionRegistry::new();
    loop {
        let message: ServerMessage =
            read_message(&mut reader).await.map_err(|error| error.to_string())?;
        match message {
            ServerMessage::Welcome { window_id, .. } => {
                // A takeover Hello's Welcome hands back the adopted window id.
                registry.adopt_window(window_id);
                tracing::debug!(adopted = ?registry.adopted_window(), "welcome: adopted window");
            }
            ServerMessage::SessionList { sessions, .. } => {
                // Rebuild the reconnect topology from the authoritative list,
                // then (on the first list) attach the single spike pane.
                registry.rebuild_from_session_list(&sessions);
                tracing::debug!(
                    sessions = registry.len(),
                    workspaces = registry.reconnect_topology().len(),
                    "rebuilt reconnect topology"
                );
                if attached.is_none() {
                    attach_first(&ctx, &sessions, &mut attached)?;
                }
            }
            ServerMessage::SessionCreated { session_id, workspace_id, .. } => {
                registry.on_session_created(session_id, workspace_id);
            }
            ServerMessage::SessionExited { session_id, .. } => {
                let existed = registry.on_session_exited(session_id);
                if existed && Some(session_id) == attached {
                    set_status(&ctx.status, &ctx.generation, "attached pane exited".to_owned());
                }
            }
            ServerMessage::TrimScrollback { session_id, history_rows } => {
                // Track the server's scrollback trim so stored prompt marks stay
                // anchored to the right rows once command-mark tracking lands.
                let dropped = registry.on_trim_scrollback(session_id, history_rows);
                tracing::trace!(%session_id, dropped, "trimmed scrollback marks");
            }
            ServerMessage::PtyOutput { session_id, data } if Some(session_id) == attached => {
                forward_output(&ctx.in_tx, session_id, data);
            }
            ServerMessage::SessionReplay { session_id, replay } if Some(session_id) == attached => {
                // A corrupt reattach stream must not tear down the reader: show
                // an error on the pane and keep the connection alive.
                match session_lifecycle::decode_replay(session_id, &replay) {
                    Ok(bytes) => forward_output(&ctx.in_tx, session_id, bytes),
                    Err(error) => {
                        set_status(&ctx.status, &ctx.generation, error.to_string());
                    }
                }
            }
            ServerMessage::ScreenSnapshot { session_id, snapshot }
                if Some(session_id) == attached =>
            {
                // Reset before replaying so the tooling snapshot replaces, never
                // appends onto, the pane's current content.
                forward_output(
                    &ctx.in_tx,
                    session_id,
                    session_lifecycle::snapshot_reset_bytes(&snapshot),
                );
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
