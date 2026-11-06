//! Tab mirrors: a floating window that keeps one tab's pane in view.
//!
//! A mirror is a second rendering of a pane's published grid, not a second
//! session. The PTY keeps the size the tab gave it, so the mirror re-wraps the
//! tab's screen onto its own columns and rows (or, on the alternate screen,
//! scales the grid to fit) instead of asking the program to redraw. Keystrokes
//! typed into it reach the same session the tab shows.
//!
//! A program on the alternate screen can offer better than a scaled copy: Pi's
//! extension renders its own screen a second time at the mirror's size and
//! streams those frames through the server ([`MirrorFeeds`]). While such a
//! feed is live the mirror paints it at its own cell count, with its own zoom
//! and its own scroll position, and the tab is untouched.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Decorations, Entity, FocusHandle,
    KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, ResizeEdge,
    Rgba, Role, ScrollWheelEvent, Size, Subscription, Task, Tiling, TitlebarOptions, Toggled,
    WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind,
    WindowOptions, div, point, prelude::*, px, size,
};
use scribe_client::input::KeyInput;
use scribe_client::keybindings::{KeyAction, LayoutAction, translate_key_action};
use scribe_client::layout::Rect;
use scribe_client::monitor;
use scribe_client::mouse_reporting;
use scribe_client::opacity::surface;
use scribe_client::restore_replay::grid_for_rect;
use scribe_client::search::MatchHighlightColors;
use scribe_client::settings::window::{
    RESIZE_GUTTER, client_tiling, resize_cursor_overlay, resize_edge,
};
use scribe_client::tab_bar::{TabBarColors, srgba};
use scribe_client::window_state::{WindowState, logical_px_to_i32};
use scribe_client::zoom::ZoomState;
use scribe_common::config::CursorShape;
use scribe_common::ids::SessionId;
use scribe_common::protocol::TerminalSize;

use crate::ipc_bridge::IpcSink;
use crate::mirror_size::{self, MirrorSize};
use crate::terminal::{
    Cell, Content, DisplayOnlyTerminal, Flags, PaneFrame, ShellCursor, ViewportPoint,
};
use crate::terminal_element::{
    CursorPaint, GridBounds, GridColors, GridFont, PaneContentView, TerminalElement, cell_at,
};
use crate::{
    PANE_BORDER_WIDTH, PANE_CARD_RADIUS, PANE_CONTENT_PADDING, TerminalView,
    codex_alt_enter_newline_bytes, encode_key_down, encode_key_up, mount_synced_view,
};

/// Height of the mirror's drag strip above its grid.
const MIRROR_HEADER_HEIGHT: f32 = 22.0;
/// Side of the strip's pin and close targets.
const MIRROR_CONTROL_SIZE: f32 = 18.0;
/// Share of the source window a fresh mirror opens at.
const MIRROR_OPEN_SCALE: f32 = 0.6;
/// Floor for the opening size, so a mirror of a small window is still usable.
const MIRROR_MIN_SIZE: (f32, f32) = (360.0, 220.0);
/// How long a resized size settles before it is written to disk.
const SIZE_SAVE_DELAY: Duration = Duration::from_millis(400);
/// How often, and how many times, the opening keep-on-top request is checked
/// and re-sent before the mirror gives up on the window manager honouring it.
const PIN_SETTLE_INTERVAL: Duration = Duration::from_millis(100);
const PIN_SETTLE_TRIES: usize = 30;

/// Whether a mirror is kept above every other window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pin {
    /// Opened above its parent window, where the platform keeps it for good.
    Fixed,
    /// Kept on top. `confirmed` is false until the window manager reports it:
    /// a request sent before the window is managed is silently dropped.
    On { confirmed: bool },
    /// Stacks like any other window.
    Off,
}

impl Pin {
    const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The grid a program renders for one session's mirror, parsed from the
/// frames its stream sends; see [`feed_mirror_frame`].
pub struct MirrorFeed {
    terminal: DisplayOnlyTerminal,
}

/// Every session with a live program-rendered mirror grid, shared between the
/// IPC reader that feeds it and the mirror window that paints it.
pub type MirrorFeeds = Arc<Mutex<HashMap<SessionId, MirrorFeed>>>;

/// Parse one streamed frame into `session_id`'s feed grid, sized to the frame.
/// Every frame after a size change is a full redraw, so resizing the grid
/// first and feeding the bytes after is consistent.
pub fn feed_mirror_frame(
    feeds: &MirrorFeeds,
    session_id: SessionId,
    size: TerminalSize,
    data: &[u8],
) {
    let Ok(mut feeds) = feeds.lock() else { return };
    let grid = (usize::from(size.cols).max(1), usize::from(size.rows).max(1));
    let feed = feeds
        .entry(session_id)
        .or_insert_with(|| MirrorFeed { terminal: DisplayOnlyTerminal::new(grid.0, grid.1) });
    if feed.terminal.dimensions() != grid {
        feed.terminal.resize(grid.0, grid.1);
    }
    feed.terminal.advance_output(data);
    feed.terminal.publish_content();
}

/// Forget `session_id`'s feed grid: its stream ended or its mirror closed.
pub fn drop_mirror_feed(feeds: &MirrorFeeds, session_id: SessionId) {
    if let Ok(mut feeds) = feeds.lock() {
        feeds.remove(&session_id);
    }
}

/// The feed grid for `session_id` if it is live and rendered at `grid`; a
/// frame at any other size is a stale one from before the last resize.
fn feed_content(
    feeds: &MirrorFeeds,
    session_id: SessionId,
    grid: (usize, usize),
) -> Option<Arc<Content>> {
    let feeds = feeds.lock().ok()?;
    let feed = feeds.get(&session_id)?;
    (feed.terminal.dimensions() == grid).then(|| feed.terminal.content())
}

/// One open mirror, as its source window tracks it.
pub struct TabMirror {
    /// The pane session the mirror shows.
    pub session_id: SessionId,
    /// The mirror's platform window.
    pub window: AnyWindowHandle,
}

/// Whether this platform lets the mirror's keep-on-top state change at runtime.
///
/// X11 window managers honour `_NET_WM_STATE_ABOVE` both ways. Wayland has no
/// always-on-top protocol and GPUI fixes a macOS window's level at creation,
/// so there the mirror is opened above its parent and stays there.
#[cfg(target_os = "linux")]
fn pin_toggle_supported() -> bool {
    gpui::guess_compositor() == "X11"
}

#[cfg(not(target_os = "linux"))]
const fn pin_toggle_supported() -> bool {
    false
}

impl TerminalView {
    /// Open a floating mirror of `tab`'s focused pane, centred over this window.
    ///
    /// The mirror stays bound to that pane session for its whole life, so
    /// moving focus to another pane or tab afterwards never retargets it.
    ///
    /// Deferred: GPUI paints a new window once inside `open_window`, and the
    /// mirror's render reads this view, which is mid-update right now.
    pub fn open_tab_mirror(&mut self, tab: SessionId, title: &str, cx: &mut Context<Self>) {
        let session_id = self.shell.tab_focused_session(tab).unwrap_or(tab);
        let bounds = mirror_bounds(self.last_window_bounds, cx);
        let parent = cx.entity();
        let title = title.to_owned();
        cx.defer(move |cx| {
            let Some(window) = open_mirror_window(&parent, (session_id, title), bounds, cx) else {
                return;
            };
            tracing::info!(%session_id, %tab, "opened a tab mirror");
            // Re-render so the next frame counts the mirrored session as placed.
            parent.update(cx, |view, cx| {
                view.mirrors.push(TabMirror { session_id, window });
                cx.notify();
            });
        });
    }
}

/// Open the mirror window itself at `bounds`, kept on top where the platform
/// allows it.
fn open_mirror_window(
    parent: &Entity<TerminalView>,
    (session_id, title): (SessionId, String),
    bounds: Bounds<Pixels>,
    cx: &mut App,
) -> Option<AnyWindowHandle> {
    let pin_toggle = pin_toggle_supported();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(format!("Mirror: {title}").into()),
                ..Default::default()
            }),
            // Without a runtime toggle the only way to keep it on top is to
            // open it above the window it mirrors.
            kind: if pin_toggle { WindowKind::Normal } else { WindowKind::Floating },
            // The mirror draws its own slim strip instead of the window
            // manager's titlebar; it falls back to the WM frame where the
            // platform refuses (X11 with no compositor).
            window_decorations: Some(WindowDecorations::Client),
            app_id: Some("scribe".to_owned()),
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        move |window, cx| {
            // GPUI's creation bounds size an X11 window but do not place it.
            monitor::apply_saved_position(
                window,
                logical_px_to_i32(f32::from(bounds.origin.x)),
                logical_px_to_i32(f32::from(bounds.origin.y)),
                WindowState::Windowed,
            );
            if pin_toggle {
                monitor::set_always_on_top(window, true);
            }
            let pin = if pin_toggle { Pin::On { confirmed: false } } else { Pin::Fixed };
            let opened = (session_id, title, bounds.size);
            let feed = {
                let source = parent.read(cx);
                (source.sink.clone(), Arc::clone(&source.shared.mirror_feeds))
            };
            let view = cx.new(|cx| MirrorView::new(parent, opened, (pin, feed), window, cx));
            let focus = view.read(cx).focus.clone();
            window.focus(&focus, cx);
            view
        },
    );
    match opened {
        Ok(handle) => Some(handle.into()),
        Err(error) => {
            tracing::warn!(%error, "could not open a tab mirror");
            None
        }
    }
}

impl TerminalView {
    /// Forget mirrors whose windows are gone; returns the sessions still shown.
    ///
    /// A mirror is a placement like any pane: its session must stay in the
    /// redraw signal's visible set, or a hidden tab's output would never wake
    /// the frame that repaints the mirror.
    pub fn live_mirror_sessions(&mut self, cx: &App) -> Vec<SessionId> {
        let open = cx.windows();
        let before: Vec<SessionId> = self.mirrors.iter().map(|mirror| mirror.session_id).collect();
        self.mirrors.retain(|mirror| {
            open.iter().any(|window| window.window_id() == mirror.window.window_id())
        });
        let live: Vec<SessionId> = self.mirrors.iter().map(|mirror| mirror.session_id).collect();
        // A session no mirror shows any more stops publishing scrollback.
        for session in before.into_iter().filter(|session| !live.contains(session)) {
            let pane = self.shared.panes.lock().ok().and_then(|panes| panes.existing(session));
            if let Some(pane) = pane {
                pane.set_history_rows(0);
            }
        }
        live
    }

    /// Route a key the mirror received to the session it shows.
    ///
    /// Bound layout and overlay chords act on this window's own panes and
    /// tabs, which the mirror is not, so of those only paste is honoured and
    /// the rest are dropped rather than encoded; terminal-shortcut bytes still
    /// reach the session. Returns whether a guarded Ctrl+Z was swallowed, so
    /// the mirror can pair the release itself.
    pub fn mirror_key_down(
        &mut self,
        session_id: SessionId,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        // An exited session must not be recreated by the handle lookup below.
        if self.shared.panes.lock().ok().and_then(|panes| panes.frame(session_id)).is_none() {
            return false;
        }
        if let Some(input) = KeyInput::from_key_down(event)
            && let Some(action) = translate_key_action(&input, self.config.bindings())
        {
            match action {
                KeyAction::Terminal(bytes) => self.send_session_key_bytes(session_id, bytes),
                KeyAction::Layout(LayoutAction::PasteClipboard) => {
                    self.paste_clipboard_into(Some(session_id), cx);
                }
                KeyAction::Layout(_)
                | KeyAction::OpenSettings
                | KeyAction::OpenCommandPalette
                | KeyAction::OpenFind => {}
            }
            return false;
        }
        if self.guards_suspend(session_id, &event.keystroke) {
            return true;
        }
        let mode = self.session_terminal_mode(session_id);
        let codex = self.session_is_codex(session_id);
        let bytes = KeyInput::from_key_down(event)
            .as_ref()
            .and_then(|input| codex_alt_enter_newline_bytes(input, codex, mode.kitty))
            .or_else(|| encode_key_down(event, mode));
        if let Some(bytes) = bytes {
            self.send_session_key_bytes(session_id, bytes);
        }
        false
    }

    /// Route a key release the mirror received, through the release encoder.
    pub fn mirror_key_up(&self, session_id: SessionId, event: &KeyUpEvent) {
        if let Some(bytes) = encode_key_up(event, self.session_terminal_mode(session_id)) {
            self.send_session_key_bytes(session_id, bytes);
        }
    }
}

/// Re-send the opening keep-on-top request until it lands or the tries run out.
async fn settle_pin(view: WeakEntity<MirrorView>, app: &mut gpui::AsyncWindowContext) {
    for _ in 0..PIN_SETTLE_TRIES {
        app.background_executor().timer(PIN_SETTLE_INTERVAL).await;
        let settled = view.update_in(app, |mirror, window, _| mirror.confirm_pin(window));
        if settled.unwrap_or(true) {
            return;
        }
    }
}

/// Where a fresh mirror opens: centred on the source window, at the size the
/// user last resized a mirror to, or else a share of the source window.
fn mirror_bounds(source: Option<Bounds<Pixels>>, cx: &App) -> Bounds<Pixels> {
    let display = source
        .and_then(|source| {
            cx.displays()
                .into_iter()
                .map(|display| display.visible_bounds())
                .find(|bounds| bounds.contains(&source.center()))
        })
        .or_else(|| cx.primary_display().map(|display| display.visible_bounds()));
    let Some(source) = source else {
        let fallback = size(px(MIRROR_MIN_SIZE.0 * 2.0), px(MIRROR_MIN_SIZE.1 * 2.0));
        let size = open_size(mirror_size::remembered(), fallback, display.map(|d| d.size));
        return Bounds::centered(None, size, cx);
    };
    let share = size(source.size.width * MIRROR_OPEN_SCALE, source.size.height * MIRROR_OPEN_SCALE);
    let size = open_size(mirror_size::remembered(), share, display.map(|d| d.size));
    let centred = Bounds::new(source.center() - point(size.width / 2.0, size.height / 2.0), size);
    display.map_or(centred, |display| keep_on_display(centred, display))
}

/// A mirror's opening size: `remembered` when the user resized one before,
/// else `default`, never below [`MIRROR_MIN_SIZE`] and never larger than the
/// display it opens on.
fn open_size(
    remembered: Option<Size<Pixels>>,
    default: Size<Pixels>,
    display: Option<Size<Pixels>>,
) -> Size<Pixels> {
    let wanted = remembered.unwrap_or(default);
    let side = |value: Pixels, floor: f32, limit: Option<Pixels>| {
        let floored = value.max(px(floor));
        limit.map_or(floored, |limit| floored.min(limit))
    };
    size(
        side(wanted.width, MIRROR_MIN_SIZE.0, display.map(|d| d.width)),
        side(wanted.height, MIRROR_MIN_SIZE.1, display.map(|d| d.height)),
    )
}

/// `bounds` shifted, not resized, so it lies inside `display` where it fits.
fn keep_on_display(bounds: Bounds<Pixels>, display: Bounds<Pixels>) -> Bounds<Pixels> {
    let axis = |origin: Pixels, extent: Pixels, start: Pixels, room: Pixels| {
        origin.min(start + room - extent).max(start)
    };
    let x = axis(bounds.origin.x, bounds.size.width, display.origin.x, display.size.width);
    let y = axis(bounds.origin.y, bounds.size.height, display.origin.y, display.size.height);
    Bounds::new(point(x, y), bounds.size)
}

/// The last source snapshot, the grid it was re-wrapped onto, and the result.
struct Wrapped {
    source: Arc<Content>,
    grid: (usize, usize),
    content: Arc<Content>,
}

/// One grid row, shared between snapshots.
type Row = Arc<Vec<Cell>>;

/// A mirror scrolled back into its pane's buffer.
///
/// The buffer is read once when the scroll begins and the view holds still
/// from then on, so text does not move under the reader while the tab keeps
/// printing; scrolling back to the bottom resumes the live view.
struct Scrollback {
    /// Every line of the pane's buffer when the scroll began, oldest first.
    source: Vec<Arc<Vec<Cell>>>,
    /// `source` re-wrapped at `cols`.
    cols: usize,
    lines: Vec<Arc<Vec<Cell>>>,
    /// Re-wrapped rows between the bottom of the view and the buffer's end.
    below: usize,
}

impl Scrollback {
    fn new(source: Vec<Arc<Vec<Cell>>>) -> Self {
        Self { source, cols: 0, lines: Vec::new(), below: 0 }
    }

    /// Re-wrap the buffer when the mirror's width changed.
    fn fit(&mut self, cols: usize) {
        if self.cols != cols {
            self.lines = reflow(self.source.iter(), None, cols).0;
            self.cols = cols;
        }
    }

    /// Walk `delta` rows back (positive) or forward on a `cols` x `rows` grid.
    /// Returns false once the view is back at the bottom.
    fn scroll(&mut self, delta: i32, (cols, rows): (usize, usize)) -> bool {
        self.fit(cols);
        let top = self.lines.len().saturating_sub(rows);
        let delta = isize::try_from(delta).unwrap_or_default();
        self.below = self.below.min(top).saturating_add_signed(delta).min(top);
        self.below > 0
    }

    fn content(&mut self, (cols, rows): (usize, usize)) -> Content {
        self.fit(cols);
        let below = self.below.min(self.lines.len().saturating_sub(rows));
        Content { rows: view_rows(&self.lines, cols, rows, below).0, ..Content::default() }
    }
}

/// The mirror window's root view.
pub struct MirrorView {
    parent: WeakEntity<TerminalView>,
    session_id: SessionId,
    title: String,
    pin: Pin,
    focus: FocusHandle,
    /// Pairs a swallowed Ctrl+Z press with its release.
    swallowed_suspend_z: bool,
    /// A left press on the strip that becomes a window move once it travels.
    move_armed: bool,
    /// The resize edge under the pointer, kept so the cursor repaints on change.
    resize_edge: Option<ResizeEdge>,
    /// The size the window opened at. Bounds reports at that size are the
    /// window being mapped or moved, not resized; cleared by the first report
    /// at any other size, after which every size is the user's.
    opened_size: Option<MirrorSize>,
    wrapped: Option<Wrapped>,
    /// Set while the wheel has walked the view back from the live bottom.
    scrollback: Option<Scrollback>,
    /// Where the grid painted last frame, so an alternate-screen wheel can be
    /// reported at the tab cell under the pointer.
    grid_bounds: GridBounds,
    content_view: Option<Entity<PaneContentView>>,
    /// This window's IPC writer, for the viewport the session's program is
    /// asked to render; kept here so the close can be sent from `Drop`.
    sink: IpcSink,
    /// Program-rendered grids, shared with the IPC reader.
    feeds: MirrorFeeds,
    /// The grid last asked of the session's program, `None` while the mirror
    /// re-wraps or scales the tab itself.
    viewport: Option<(usize, usize)>,
    /// The mirror's own font step, independent of the tab's zoom.
    zoom: ZoomState,
    _subscriptions: [Subscription; 3],
    _pin_settle: Task<()>,
}

impl Drop for MirrorView {
    fn drop(&mut self) {
        self.set_viewport(None);
    }
}

impl MirrorView {
    fn new(
        parent: &Entity<TerminalView>,
        (session_id, title, opened): (SessionId, String, Size<Pixels>),
        (pin, (sink, feeds)): (Pin, (IpcSink, MirrorFeeds)),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The source view notifies whenever a placed session's grid moves, and
        // the mirror's session counts as placed.
        let repaint = cx.observe(parent, |_, _, cx| cx.notify());
        // A closing source window takes its mirrors with it; otherwise the
        // last mirror would keep the process alive after every terminal left.
        let close = cx.observe_release_in(parent, window, |_, _, window, _| window.remove_window());
        let pin_settle = cx.spawn_in(window, async move |view, app| settle_pin(view, app).await);
        let resized =
            cx.observe_window_bounds(window, |this, window, cx| this.on_bounds(window, cx));
        Self {
            parent: parent.downgrade(),
            session_id,
            title,
            pin,
            focus: cx.focus_handle(),
            swallowed_suspend_z: false,
            move_armed: false,
            resize_edge: None,
            opened_size: MirrorSize::of(opened),
            wrapped: None,
            scrollback: None,
            grid_bounds: GridBounds::default(),
            content_view: None,
            sink,
            feeds,
            viewport: None,
            zoom: ZoomState::new(),
            _subscriptions: [repaint, close, resized],
            _pin_settle: pin_settle,
        }
    }

    /// Ask the session's program for frames at `grid`, or for none, when that
    /// differs from what it was last asked. A `None` also drops the feed grid,
    /// so a stale frame cannot paint after the mirror went back to scaling.
    fn set_viewport(&mut self, grid: Option<(usize, usize)>) {
        if self.viewport == grid {
            return;
        }
        self.viewport = grid;
        let sent = if let Some((cols, rows)) = grid {
            let size = TerminalSize {
                cols: u16::try_from(cols).unwrap_or(u16::MAX),
                rows: u16::try_from(rows).unwrap_or(u16::MAX),
                ..TerminalSize::default()
            };
            self.sink.mirror_viewport(self.session_id, size)
        } else {
            drop_mirror_feed(&self.feeds, self.session_id);
            self.sink.mirror_close(self.session_id)
        };
        tracing::info!(session = %self.session_id, ?grid, "tab mirror viewport");
        if let Err(error) = sent {
            tracing::warn!(%error, "mirror viewport dropped: IPC writer closed");
        }
    }

    /// Whether a program-rendered grid at the asked viewport is live.
    fn feed_live(&self) -> bool {
        self.viewport.is_some_and(|grid| feed_content(&self.feeds, self.session_id, grid).is_some())
    }

    /// The tab's font at the mirror's own zoom step.
    fn zoomed(&self, font: &GridFont) -> GridFont {
        let size = self.zoom.effective_font_size(font.size);
        let scale = if font.size > 0.0 { size / font.size } else { 1.0 };
        GridFont { size, line_height: font.line_height * scale, ..font.clone() }
    }

    /// A zoom chord typed into the mirror steps the mirror's own font.
    fn apply_zoom(&mut self, action: LayoutAction, cx: &mut Context<Self>) {
        match action {
            LayoutAction::ZoomIn => self.zoom.zoom_in(),
            LayoutAction::ZoomOut => self.zoom.zoom_out(),
            LayoutAction::ZoomReset => self.zoom.reset(),
            _ => return,
        }
        let level = self.zoom.level();
        tracing::info!(session = %self.session_id, level, "tab mirror zoom changed");
        cx.notify();
    }

    /// Remember a user resize so the next mirror opens at the same size. A
    /// maximized, fullscreen, or tiled window's size is the compositor's, and
    /// a move changes no size, so neither is remembered.
    fn on_bounds(&mut self, window: &Window, cx: &mut Context<Self>) {
        let tiled = client_tiling(window.window_decorations()).is_tiled();
        if window.is_maximized() || window.is_fullscreen() || tiled {
            return;
        }
        let Some(size) = MirrorSize::of(window.bounds().size) else { return };
        if self.opened_size == Some(size) {
            return;
        }
        self.opened_size = None;
        if mirror_size::remember(size) {
            // One write per settle window, however many frames a drag-resize
            // reports; detached so closing the mirror cannot cancel it.
            let executor = cx.background_executor().clone();
            cx.background_spawn(async move {
                executor.timer(SIZE_SAVE_DELAY).await;
                mirror_size::flush();
            })
            .detach();
        }
    }

    fn toggle_pin(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.pin = match self.pin {
            Pin::Fixed => return,
            Pin::On { .. } => Pin::Off,
            Pin::Off => Pin::On { confirmed: true },
        };
        let pinned = self.pin.is_on();
        let sent = monitor::set_always_on_top(window, pinned);
        tracing::info!(session = %self.session_id, pinned, sent, "tab mirror pin toggled");
        cx.notify();
    }

    /// Re-send the opening keep-on-top request until the window manager
    /// reports it. Returns whether there is nothing left to settle: confirmed,
    /// toggled since, or unsupported here.
    fn confirm_pin(&mut self, window: &Window) -> bool {
        if self.pin != (Pin::On { confirmed: false }) {
            return true;
        }
        let confirmed =
            monitor::is_always_on_top(window) || !monitor::set_always_on_top(window, true);
        self.pin = Pin::On { confirmed };
        confirmed
    }

    /// The grid snapshot to paint at `cols` x `rows`, re-wrapped once per new
    /// source snapshot (every snapshot carries a fresh history alongside it).
    fn wrapped_content(&mut self, frame: &PaneFrame, grid: (usize, usize)) -> Arc<Content> {
        let source = &frame.content;
        if let Some(seen) = &self.wrapped
            && Arc::ptr_eq(&seen.source, source)
            && seen.grid == grid
        {
            return Arc::clone(&seen.content);
        }
        let content = Arc::new(rewrap(&frame.history, source, grid.0, grid.1));
        self.wrapped =
            Some(Wrapped { source: Arc::clone(source), grid, content: Arc::clone(&content) });
        content
    }

    /// The slim strip: a dim title, then pin and close. Pressing anywhere on it
    /// outside those controls and the resize corners moves the window.
    fn render_header(
        &self,
        colors: &TabBarColors,
        tiling: Tiling,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pinned = self.pin.is_on();
        let hover = Rgba { a: 0.12, ..colors.text };
        let pin = (self.pin != Pin::Fixed).then(|| {
            let color = if pinned { colors.accent } else { colors.text };
            strip_control("mirror-pin", hover)
                .role(Role::Switch)
                .aria_label("Keep this mirror on top")
                .aria_toggled(if pinned { Toggled::True } else { Toggled::False })
                .child(pin_icon(color))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_pin(window, cx)))
        });
        let close = strip_control("mirror-close", hover)
            .role(Role::Button)
            .aria_label("Close this mirror")
            .text_color(colors.text)
            .child("\u{00D7}")
            .on_click(cx.listener(|this, _, window, _| {
                tracing::info!(session = %this.session_id, "tab mirror closed from its strip");
                window.remove_window();
            }));
        div()
            .id("mirror-strip")
            .role(Role::TitleBar)
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .h(px(MIRROR_HEADER_HEIGHT))
            .pl_2()
            .pr_1()
            .border_b_1()
            .border_color(colors.pane_edge)
            .text_xs()
            .text_color(colors.text)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, _| {
                    let window_size = window.window_bounds().get_bounds().size;
                    this.move_armed =
                        resize_edge(event.position, RESIZE_GUTTER, window_size, tiling).is_none();
                }),
            )
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.move_armed = false))
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.move_armed = false))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
                // Linux backends ignore `WindowControlArea::Drag`, so the move
                // is started explicitly once a held press travels.
                if this.move_armed && event.pressed_button == Some(MouseButton::Left) {
                    this.move_armed = false;
                    window.start_window_move();
                }
            }))
            .child(div().flex_1().truncate().child(if self.scrollback.is_some() {
                format!("{} (scrolled back)", self.title)
            } else {
                self.title.clone()
            }))
            .children(pin)
            .child(close)
            .into_any_element()
    }

    /// Wrap the card in the client-side frame: a transparent resize gutter on
    /// every untiled side and a rounded outline. Under server decorations the
    /// window manager owns the frame, so the card fills the window plainly.
    fn render_frame(
        decorations: Decorations,
        card: gpui::Div,
        outline: Rgba,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let frame = div().id("mirror-frame").size_full();
        let Decorations::Client { tiling } = decorations else {
            return frame.child(card);
        };
        let card = card
            .border(px(PANE_BORDER_WIDTH))
            .border_color(outline)
            .when(!tiling.is_tiled(), |card| card.rounded(px(PANE_CARD_RADIUS)));
        frame
            .when(!tiling.top, |this| this.pt(RESIZE_GUTTER))
            .when(!tiling.bottom, |this| this.pb(RESIZE_GUTTER))
            .when(!tiling.left, |this| this.pl(RESIZE_GUTTER))
            .when(!tiling.right, |this| this.pr(RESIZE_GUTTER))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                let window_size = window.window_bounds().get_bounds().size;
                let edge = resize_edge(event.position, RESIZE_GUTTER, window_size, tiling);
                if edge != this.resize_edge {
                    this.resize_edge = edge;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, window, _| {
                let window_size = window.window_bounds().get_bounds().size;
                if let Some(edge) = resize_edge(event.position, RESIZE_GUTTER, window_size, tiling)
                {
                    window.start_window_resize(edge);
                }
            })
            .child(card)
            .child(resize_cursor_overlay(tiling))
    }

    /// On the main screen, walk the view back through the pane's whole buffer
    /// or forward to the live bottom. On the alternate screen the mirror shows
    /// the tab's own grid and the program owns any history, so the wheel goes
    /// to the program exactly as the tab would send it.
    fn on_scroll(&mut self, event: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(parent) = self.parent.upgrade() else { return };
        let source = parent.read(cx);
        let natural = source.config.config().config.terminal.scroll.natural_scroll;
        let rows = mouse_reporting::wheel_lines(event.delta, source.font.line_height, natural);
        let Some(frame) =
            source.shared.panes.lock().ok().and_then(|panes| panes.frame(self.session_id))
        else {
            return;
        };
        if rows == 0 {
            return;
        }
        let (width, height) = grid_extent(window);
        // A program rendering the mirror itself owns the mirror's scroll.
        if frame.alt_screen && self.feed_live() {
            if let Err(error) = self.sink.mirror_scroll(self.session_id, rows) {
                tracing::warn!(%error, "mirror scroll dropped: IPC writer closed");
            }
            return;
        }
        if frame.alt_screen {
            let font = fit_font(&source.font, frame.dimensions, (width, height));
            let cell = self
                .grid_bounds
                .get()
                .and_then(|bounds| report_cell(bounds, &font, event.position, frame.dimensions));
            source.wheel_to_program(self.session_id, rows, cell, event.modifiers);
            return;
        }
        let grid = grid_cells(&source.font, width, height);
        let scrolled = match self.scrollback.as_mut() {
            Some(scrollback) => scrollback.scroll(rows, grid),
            None if rows > 0 => {
                let pane =
                    source.shared.panes.lock().ok().and_then(|p| p.existing(self.session_id));
                let Some(buffer) = pane.and_then(|pane| pane.with_terminal(|t| t.buffer_rows()))
                else {
                    return;
                };
                self.scrollback.insert(Scrollback::new(buffer)).scroll(rows, grid)
            }
            None => return,
        };
        if !scrolled {
            self.scrollback = None;
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // Typing returns to the live bottom, as a terminal's scrollback does.
        if self.scrollback.take().is_some() {
            cx.notify();
        }
        let Some(parent) = self.parent.upgrade() else { return };
        let zoom = KeyInput::from_key_down(event)
            .and_then(|input| translate_key_action(&input, parent.read(cx).config.bindings()));
        if let Some(KeyAction::Layout(
            action @ (LayoutAction::ZoomIn | LayoutAction::ZoomOut | LayoutAction::ZoomReset),
        )) = zoom
        {
            self.apply_zoom(action, cx);
            return;
        }
        let session = self.session_id;
        if !event.is_held && event.keystroke.key.eq_ignore_ascii_case("z") {
            self.swallowed_suspend_z = false;
        }
        if parent.update(cx, |view, cx| view.mirror_key_down(session, event, cx)) {
            self.swallowed_suspend_z = true;
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if event.keystroke.key.eq_ignore_ascii_case("z") && self.swallowed_suspend_z {
            self.swallowed_suspend_z = false;
            return;
        }
        let Some(parent) = self.parent.upgrade() else { return };
        parent.read(cx).mirror_key_up(self.session_id, event);
    }

    /// The grid for this frame: re-wrapped onto the mirror's own cells; on the
    /// alternate screen the program's own rendering at the mirror's size when
    /// it streams one, else the tab's grid with the font scaled to fit.
    fn grid(
        &mut self,
        frame: &PaneFrame,
        style: &MirrorStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (width, height) = grid_extent(window);
        if frame.alt_screen {
            self.scrollback = None;
        }
        let own = self.zoomed(&style.font);
        let grid = grid_cells(&own, width, height);
        self.set_viewport(frame.alt_screen.then_some(grid));
        let (content, font) = if frame.alt_screen {
            feed_content(&self.feeds, self.session_id, grid).map_or_else(
                || {
                    let fitted = fit_font(&style.font, frame.dimensions, (width, height));
                    (Arc::clone(&frame.content), fitted)
                },
                |content| (content, own.clone()),
            )
        } else if let Some(scrollback) = self.scrollback.as_mut() {
            (Arc::new(scrollback.content(grid)), own)
        } else {
            (self.wrapped_content(frame, grid), own)
        };
        let mut element = TerminalElement::new(
            content,
            font,
            style.colors.clone(),
            style.highlight,
            Rc::clone(&self.grid_bounds),
        )
        .with_cursor(Some(CursorPaint { visible: true, shape: style.cursor }));
        let view = self
            .content_view
            .get_or_insert_with(|| cx.new(|_| PaneContentView::new(element.clone())))
            .clone();
        let changed = view.update(cx, |view, _| view.sync(&mut element));
        div()
            .relative()
            .size_full()
            .child(mount_synced_view(view, changed))
            .child(div().absolute().inset_0().child(element.paint_overlay()))
            .into_any_element()
    }
}

/// Everything the mirror paints with, read off its source view each frame so
/// theme, font, and zoom edits reach it the moment they reach the tab.
struct MirrorStyle {
    font: GridFont,
    colors: GridColors,
    highlight: MatchHighlightColors,
    header: TabBarColors,
    /// The card's outline: the pane divider at full strength, because a
    /// floating mirror sits over other windows rather than the window ground.
    edge: Rgba,
    cursor: CursorShape,
}

impl MirrorStyle {
    fn of(source: &TerminalView) -> Self {
        let opacity = source.opacity;
        Self {
            font: source.font.clone(),
            colors: GridColors {
                background: surface(source.terminal_colors.background, opacity),
                cells: Arc::clone(&source.terminal_colors.cells),
                opacity,
            },
            highlight: source.highlight_colors,
            header: TabBarColors::from_chrome(&source.chrome, opacity),
            edge: srgba(source.chrome.divider),
            cursor: source.config.config().config.appearance.cursor_shape,
        }
    }
}

impl Render for MirrorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(parent) = self.parent.upgrade() else {
            window.remove_window();
            return div().into_any_element();
        };
        let source = parent.read(cx);
        // The strip is the reader's record of every open session, panes
        // included, and drops one before the repaint its exit requests. The
        // grid alone cannot say: a late output batch for an exited session
        // recreates its grid, which kept a mirror of a split pane open.
        let open = source
            .shared
            .tabs
            .lock()
            .is_ok_and(|tabs| tabs.workspace_of(self.session_id).is_some());
        let frame = open
            .then(|| source.shared.panes.lock().ok().and_then(|panes| panes.frame(self.session_id)))
            .flatten();
        let Some(frame) = frame else {
            // The tab closed, exited, or moved to another window.
            tracing::info!(session = %self.session_id, "tab mirror closed: its session left");
            window.remove_window();
            return div().into_any_element();
        };
        let style = MirrorStyle::of(source);
        // A mirror taller than the tab fills from the scrollback above the
        // tab's screen, the way a taller terminal would show it. Only ever
        // grown here; the source resets it once no mirror shows the session.
        let (width, height) = grid_extent(window);
        let own = self.zoomed(&style.font);
        let wanted = history_rows_wanted(grid_cells(&own, width, height), frame.dimensions.0);
        if !frame.alt_screen && frame.history_rows < wanted {
            let pane = source.shared.panes.lock().ok().and_then(|p| p.existing(self.session_id));
            if let Some(pane) = pane {
                pane.set_history_rows(wanted);
                window.request_animation_frame();
            }
        }
        let decorations = window.window_decorations();
        window.set_client_inset(match decorations {
            Decorations::Client { .. } => RESIZE_GUTTER,
            Decorations::Server => px(0.0),
        });
        let header = self.render_header(&style.header, client_tiling(decorations), cx);
        let grid = self.grid(&frame, &style, window, cx);
        let card = div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(style.colors.background)
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .p(px(PANE_CONTENT_PADDING))
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, win, cx| {
                        this.on_scroll(event, win, cx);
                    }))
                    .child(grid),
            );
        Self::render_frame(decorations, card, style.edge, cx)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.on_key_down(event, cx);
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, cx| {
                this.on_key_up(event, cx);
            }))
            .into_any_element()
    }
}

/// Scrollback rows that always fill a `cols` x `rows` mirror of a pane
/// `source_cols` wide: a wider mirror joins soft-wrapped rows, so each of its
/// rows can absorb up to `cols / source_cols` (rounded up) of the pane's.
fn history_rows_wanted((cols, rows): (usize, usize), source_cols: usize) -> usize {
    rows * cols.div_ceil(source_cols.max(1)).max(1)
}

/// The mirror's grid area in pixels: the window minus its frame, strip, and
/// padding.
fn grid_extent(window: &Window) -> (f32, f32) {
    let viewport = window.viewport_size();
    let (frame_w, frame_h) = match window.window_decorations() {
        Decorations::Server => (0.0, 0.0),
        Decorations::Client { tiling } => {
            let side = |tiled: bool| if tiled { 0.0 } else { f32::from(RESIZE_GUTTER) };
            (
                side(tiling.left) + side(tiling.right) + 2.0 * PANE_BORDER_WIDTH,
                side(tiling.top) + side(tiling.bottom) + 2.0 * PANE_BORDER_WIDTH,
            )
        }
    };
    (
        f32::from(viewport.width) - frame_w - 2.0 * PANE_CONTENT_PADDING,
        f32::from(viewport.height) - frame_h - MIRROR_HEADER_HEIGHT - 2.0 * PANE_CONTENT_PADDING,
    )
}

/// A square strip target with a hover fill. Its press stops at the target so
/// it never arms the strip's window move.
fn strip_control(id: &'static str, hover: Rgba) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(MIRROR_CONTROL_SIZE))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Whole cells that fit a `width` x `height` pixel grid at `font`, through the
/// client's one grid formula.
fn grid_cells(font: &GridFont, width: f32, height: f32) -> (usize, usize) {
    let rect = Rect { x: 0.0, y: 0.0, width, height };
    let grid = grid_for_rect(rect, (font.cell_width(), font.line_height));
    (usize::from(grid.cols).max(2), usize::from(grid.rows).max(1))
}

/// `font` scaled so a `cols` x `rows` grid fits `width` x `height` pixels.
///
/// An alternate-screen program drew its layout for the tab's exact size, so
/// re-wrapping it would scramble it; shrinking or growing the glyphs keeps it
/// whole.
fn fit_font(
    font: &GridFont,
    (cols, rows): (usize, usize),
    (width, height): (f32, f32),
) -> GridFont {
    let natural_width = cell_count_f32(cols) * font.cell_width();
    let natural_height = cell_count_f32(rows) * font.line_height;
    let scale = (width / natural_width).min(height / natural_height);
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    GridFont { size: font.size * scale, line_height: font.line_height * scale, ..font.clone() }
}

/// The tab cell a mouse report names for `position` over an alternate-screen
/// grid painted at `bounds` with `font`, clamped onto the tab's `cols` x
/// `rows`: the fitted grid leaves spare room on one axis, and a wheel there
/// still belongs to the program.
fn report_cell(
    bounds: Bounds<Pixels>,
    font: &GridFont,
    position: gpui::Point<Pixels>,
    (cols, rows): (usize, usize),
) -> Option<(u16, u16)> {
    let at = cell_at(bounds, font, position)?;
    let axis = |value: usize, len: usize| u16::try_from(value.min(len.max(1) - 1)).ok();
    Some((axis(at.col, cols)?, axis(at.row, rows)?))
}

fn cell_count_f32(count: usize) -> f32 {
    f32::from(u16::try_from(count).unwrap_or(u16::MAX))
}

/// Keep-on-top mark: a push pin, cap over body over a short needle.
fn pin_icon(color: Rgba) -> gpui::AnyElement {
    div()
        .relative()
        .flex_none()
        .size(px(12.0))
        .child(div().absolute().left(px(2.0)).top(px(1.0)).w(px(8.0)).h(px(2.0)).bg(color))
        .child(div().absolute().left(px(4.0)).top(px(3.0)).w(px(4.0)).h(px(4.0)).bg(color))
        .child(div().absolute().left(px(1.0)).top(px(7.0)).w(px(10.0)).h(px(1.0)).bg(color))
        .child(div().absolute().left(px(5.5)).top(px(8.0)).w(px(1.0)).h(px(4.0)).bg(color))
        .into_any_element()
}

/// Whether `cell` only pads its row out to the terminal width.
fn is_padding(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == vte::ansi::Color::Named(vte::ansi::NamedColor::Background)
        && !cell.flags.intersects(
            Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT | Flags::WIDE_CHAR_SPACER,
        )
}

/// Re-wrap a pane snapshot, preceded by `history` rows of its scrollback, onto
/// a `cols` x `rows` grid.
///
/// Rows the terminal soft-wrapped (`WRAPLINE` on their last cell) are joined
/// back into logical lines, each line's trailing padding is dropped, and the
/// lines are re-chunked at the new width. A wide character never splits across
/// rows. Blank lines below the cursor are dropped, then the bottom `rows` rows
/// are kept, so the live line stays in view when the mirror is shorter than
/// the re-wrapped screen, and scrollback fills a mirror taller than it. The
/// shell cursor follows its cell through the re-wrap.
pub fn rewrap(history: &[Arc<Vec<Cell>>], content: &Content, cols: usize, rows: usize) -> Content {
    let cols = cols.max(2);
    let rows = rows.max(1);
    let cursor = content.shell_cursor;
    let at = cursor.map(|cursor| (cursor.point.row + history.len(), cursor.point.col));
    let (out, out_cursor) = reflow(history.iter().chain(&content.rows), at, cols);
    let (visible, start) = view_rows(&out, cols, rows, 0);
    let shell_cursor = cursor.zip(out_cursor).and_then(|(cursor, (row, col))| {
        let row = row.checked_sub(start)?;
        Some(ShellCursor { point: ViewportPoint { row, col }, shape: cursor.shape })
    });
    Content { rows: visible, display_offset: 0, pin_rows: 0, vi_cursor: None, shell_cursor }
}

/// `rows` re-wrapped rows ending `below` rows above the bottom of `lines`,
/// padded with blank rows when `lines` is shorter, and the index of the first.
fn view_rows(lines: &[Row], cols: usize, rows: usize, below: usize) -> (Vec<Row>, usize) {
    let end = lines.len().saturating_sub(below);
    let start = end.saturating_sub(rows);
    let mut visible: Vec<Row> = lines.iter().take(end).skip(start).cloned().collect();
    let blank = Arc::new(vec![Cell::default(); cols]);
    visible.resize_with(rows, || Arc::clone(&blank));
    (visible, start)
}

/// Join `rows` into logical lines and re-chunk them at `cols`, returning every
/// re-wrapped row and where `cursor` (row, column in `rows`) landed.
fn reflow<'a>(
    rows: impl Iterator<Item = &'a Arc<Vec<Cell>>>,
    cursor: Option<(usize, usize)>,
    cols: usize,
) -> (Vec<Row>, Option<(usize, usize)>) {
    // Logical lines, each with the cursor's offset when it sits on that line.
    let mut lines: Vec<(Vec<Cell>, Option<usize>)> = Vec::new();
    let mut line: Vec<Cell> = Vec::new();
    let mut line_cursor = None;
    for (row_index, row) in rows.enumerate() {
        if let Some((_, col)) = cursor.filter(|(cursor_row, _)| *cursor_row == row_index) {
            line_cursor = Some(line.len() + col);
        }
        let wraps = row.last().is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        line.extend(
            row.iter().filter(|cell| !cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)),
        );
        if !wraps {
            lines.push((std::mem::take(&mut line), line_cursor.take()));
        }
    }
    if !line.is_empty() {
        lines.push((line, line_cursor));
    }
    for (cells, at) in &mut lines {
        let keep = at.map_or(0, |at| at + 1);
        while cells.len() > keep && cells.last().is_some_and(is_padding) {
            cells.pop();
        }
        for cell in cells.iter_mut() {
            cell.flags.remove(Flags::WRAPLINE);
        }
    }
    let used = lines.iter().rposition(|(cells, at)| !cells.is_empty() || at.is_some());
    lines.truncate(used.map_or(0, |last| last + 1));

    let mut out: Vec<Vec<Cell>> = Vec::new();
    let mut out_cursor = None;
    let pad = |row: &mut Vec<Cell>| row.resize(cols, Cell::default());
    for (cells, at) in lines {
        let mut row = Vec::with_capacity(cols);
        for (index, cell) in cells.iter().enumerate() {
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            if row.len() == cols || (wide && row.len() + 1 == cols) {
                pad(&mut row);
                out.push(std::mem::replace(&mut row, Vec::with_capacity(cols)));
            }
            if at == Some(index) {
                out_cursor = Some((out.len(), row.len()));
            }
            row.push(*cell);
        }
        if at.is_some_and(|at| at >= cells.len()) {
            out_cursor = Some((out.len(), row.len().min(cols - 1)));
        }
        pad(&mut row);
        out.push(row);
    }
    (out.into_iter().map(Arc::new).collect(), out_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ShellCursorShape;

    /// A snapshot `cols` wide from `rows`; a trailing `\` marks a soft wrap.
    fn snapshot(cols: usize, rows: &[&str], cursor: Option<(usize, usize)>) -> Content {
        let rows = rows
            .iter()
            .map(|text| {
                let wraps = text.ends_with('\\');
                let mut cells: Vec<Cell> = text
                    .trim_end_matches('\\')
                    .chars()
                    .map(|c| Cell { c, ..Cell::default() })
                    .collect();
                cells.resize(cols, Cell::default());
                if wraps && let Some(last) = cells.last_mut() {
                    last.flags.insert(Flags::WRAPLINE);
                }
                Arc::new(cells)
            })
            .collect();
        Content {
            rows,
            shell_cursor: cursor.map(|(row, col)| ShellCursor {
                point: ViewportPoint { row, col },
                shape: ShellCursorShape::Block,
            }),
            ..Content::default()
        }
    }

    fn texts(content: &Content) -> Vec<String> {
        content
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.c).collect::<String>().trim_end().to_owned())
            .collect()
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#Narrowing re-wraps a line and follows the cursor]]
    #[test]
    fn narrowing_rewraps_a_line_and_follows_the_cursor() {
        let source = snapshot(10, &["$ echo abc", "abc", "$ "], Some((2, 2)));

        let wrapped = rewrap(&[], &source, 4, 6);

        assert_eq!(texts(&wrapped), ["$ ec", "ho a", "bc", "abc", "$", ""]);
        let cursor = wrapped.shell_cursor.expect("the cursor survives the re-wrap");
        assert_eq!((cursor.point.row, cursor.point.col), (4, 2));
        assert!(wrapped.rows.iter().all(|row| row.len() == 4), "every row is grid width");
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#Widening joins soft-wrapped rows]]
    #[test]
    fn widening_joins_soft_wrapped_rows() {
        let source = snapshot(4, &["abcd\\", "ef", "gh"], None);

        let wrapped = rewrap(&[], &source, 8, 3);

        assert_eq!(texts(&wrapped), ["abcdef", "gh", ""]);
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#A short mirror keeps the live bottom]]
    #[test]
    fn a_short_mirror_keeps_the_live_bottom() {
        let source = snapshot(6, &["one", "two", "three", "$ ", "", ""], Some((3, 2)));

        let wrapped = rewrap(&[], &source, 6, 2);

        assert_eq!(texts(&wrapped), ["three", "$"], "blank rows below the cursor are dropped");
        let cursor = wrapped.shell_cursor.expect("the live line stays in view");
        assert_eq!((cursor.point.row, cursor.point.col), (1, 2));
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#A wide character never splits]]
    #[test]
    fn a_wide_character_never_splits() {
        let mut source = snapshot(6, &["ab\u{754c}"], None);
        let row = Arc::make_mut(&mut source.rows[0]);
        row[2].flags.insert(Flags::WIDE_CHAR);
        row[3] = Cell { c: ' ', flags: Flags::WIDE_CHAR_SPACER, ..Cell::default() };

        let wrapped = rewrap(&[], &source, 3, 2);

        assert_eq!(texts(&wrapped), ["ab", "\u{754c}"]);
        assert!(wrapped.rows[1][0].flags.contains(Flags::WIDE_CHAR));
        assert!(wrapped.rows[1][1].flags.contains(Flags::WIDE_CHAR_SPACER));
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#Scrolling back walks the whole buffer]]
    #[test]
    fn scrolling_back_walks_the_whole_buffer() {
        let mut terminal = crate::terminal::DisplayOnlyTerminal::new(6, 3);
        let body = (1..=8).map(|n| format!("l{n}")).collect::<Vec<_>>().join("\r\n");
        terminal.feed_output(body.as_bytes());
        // The tab scrolled to its oldest line still hands over its whole buffer.
        assert!(terminal.scroll_to_abs(0));
        let mut scrollback = Scrollback::new(terminal.buffer_rows());

        assert!(scrollback.scroll(3, (6, 2)));
        assert_eq!(texts(&scrollback.content((6, 2))), ["l4", "l5"]);
        assert!(scrollback.scroll(100, (6, 2)), "held at the oldest line");
        assert_eq!(texts(&scrollback.content((6, 2))), ["l1", "l2"]);
        assert!(!scrollback.scroll(-100, (6, 2)), "back at the bottom: the live view resumes");
        assert_eq!(texts(&scrollback.content((6, 2))), ["l7", "l8"]);
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#An alternate-screen wheel reports a tab cell]]
    #[test]
    fn an_alternate_screen_wheel_reports_a_tab_cell() {
        let font = GridFont::default();
        let (cw, lh) = (font.cell_width(), font.line_height);
        // A fitted 4 x 2 grid in an area with spare room below it.
        let bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(cw * 4.0), px(lh * 5.0)));
        let at = |x: f32, y: f32| point(px(10.0 + x), px(20.0 + y));

        assert_eq!(report_cell(bounds, &font, at(cw * 2.5, lh * 0.5), (4, 2)), Some((2, 0)));
        assert_eq!(
            report_cell(bounds, &font, at(cw * 0.5, lh * 4.5), (4, 2)),
            Some((0, 1)),
            "spare room below the fitted grid clamps onto its last row"
        );
        assert_eq!(report_cell(bounds, &font, at(-1.0, 0.0), (4, 2)), None, "off the grid");
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror size#A remembered size opens on the display]]
    #[test]
    fn a_remembered_size_opens_on_the_display() {
        let share = size(px(600.0), px(400.0));
        let display = Some(size(px(1280.0), px(800.0)));
        let resized = size(px(900.0), px(500.0));

        assert_eq!(open_size(None, share, display), share, "no resize yet: the share");
        assert_eq!(open_size(Some(resized), share, display), resized, "a resize wins");
        assert_eq!(
            open_size(Some(size(px(2000.0), px(1200.0))), share, display),
            size(px(1280.0), px(800.0)),
            "a size saved on a larger display is capped to this one"
        );
        assert_eq!(
            open_size(Some(size(px(100.0), px(100.0))), share, display),
            size(px(MIRROR_MIN_SIZE.0), px(MIRROR_MIN_SIZE.1)),
            "the floor still applies"
        );

        let screen = Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(800.0)));
        let overhanging = Bounds::new(point(px(1000.0), px(-50.0)), size(px(600.0), px(400.0)));
        let kept = keep_on_display(overhanging, screen);
        assert_eq!(kept.origin, point(px(680.0), px(0.0)), "shifted back onto the display");
        assert_eq!(kept.size, overhanging.size, "shifting never resizes");
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#Scrollback fills a mirror taller than the tab]]
    #[test]
    fn scrollback_fills_a_mirror_taller_than_the_tab() {
        let history = snapshot(6, &["old 1", "old 2\\", "tail"], None).rows;
        let screen = snapshot(6, &["now", "$ "], Some((1, 2)));

        let wrapped = rewrap(&history, &screen, 12, 4);

        assert_eq!(texts(&wrapped), ["old 1", "old 2 tail", "now", "$"]);
        let cursor = wrapped.shell_cursor.expect("the cursor stays on the live line");
        assert_eq!((cursor.point.row, cursor.point.col), (3, 2));
        assert_eq!(history_rows_wanted((12, 4), 6), 8, "a wider mirror asks for joined rows");
    }

    // @lat: [[test#GPUI Client Headless Suites#Tab mirror re-wrap#A streamed frame paints at its own size]]
    #[test]
    fn a_streamed_frame_paints_at_its_own_size() {
        let feeds: MirrorFeeds = Arc::default();
        let session = SessionId::new();
        let wide = TerminalSize { cols: 12, rows: 3, ..TerminalSize::default() };
        // Pi's compositor writes one synchronized full redraw per frame.
        let frame = b"\x1b[?2026h\x1b[2J\x1b[1;1H\x1b[2Khello mirror\x1b[2;1H\x1b[2K> typed\x1b[2;8H\x1b[?25h\x1b[?2026l";
        feed_mirror_frame(&feeds, session, wide, frame);

        let content = feed_content(&feeds, session, (12, 3)).expect("the frame's grid is live");
        assert_eq!(texts(&content), ["hello mirror", "> typed", ""]);
        let cursor = content.shell_cursor.expect("the frame placed the cursor");
        assert_eq!((cursor.point.row, cursor.point.col), (1, 7));
        assert!(feed_content(&feeds, session, (80, 24)).is_none(), "another size is stale");

        // A frame for a new size regrows the grid before it is drawn.
        let narrow = TerminalSize { cols: 6, rows: 2, ..TerminalSize::default() };
        feed_mirror_frame(
            &feeds,
            session,
            narrow,
            b"\x1b[?2026h\x1b[2J\x1b[1;1H\x1b[2Ksmall\x1b[?2026l",
        );
        let resized = feed_content(&feeds, session, (6, 2)).expect("resized grid is live");
        assert_eq!(texts(&resized), ["small", ""]);

        drop_mirror_feed(&feeds, session);
        assert!(feed_content(&feeds, session, (6, 2)).is_none(), "a dropped feed paints nothing");
    }
}
