//! Tab mirrors: a floating window that keeps one tab's pane in view.
//!
//! A mirror is a second rendering of a pane's published grid, not a second
//! session. The PTY keeps the size the tab gave it, so the mirror re-wraps the
//! tab's screen onto its own columns and rows (or, on the alternate screen,
//! scales the grid to fit) instead of asking the program to redraw. Keystrokes
//! typed into it reach the same session the tab shows.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Decorations, Entity, FocusHandle,
    KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, ResizeEdge,
    Rgba, Role, Size, Subscription, Task, Tiling, TitlebarOptions, Toggled, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions, div,
    point, prelude::*, px, size,
};
use scribe_client::input::KeyInput;
use scribe_client::keybindings::{KeyAction, LayoutAction, translate_key_action};
use scribe_client::layout::Rect;
use scribe_client::monitor;
use scribe_client::opacity::surface;
use scribe_client::restore_replay::grid_for_rect;
use scribe_client::search::MatchHighlightColors;
use scribe_client::settings::window::{
    RESIZE_GUTTER, client_tiling, resize_cursor_overlay, resize_edge,
};
use scribe_client::tab_bar::{TabBarColors, srgba};
use scribe_client::window_state::{WindowState, logical_px_to_i32};
use scribe_common::config::CursorShape;
use scribe_common::ids::SessionId;

use crate::mirror_size::{self, MirrorSize};
use crate::terminal::{Cell, Content, Flags, PaneFrame, ShellCursor, ViewportPoint};
use crate::terminal_element::{
    CursorPaint, GridColors, GridFont, PaneContentView, TerminalElement,
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
            let view = cx.new(|cx| MirrorView::new(parent, opened, pin, window, cx));
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
    content_view: Option<Entity<PaneContentView>>,
    _subscriptions: [Subscription; 3],
    _pin_settle: Task<()>,
}

impl MirrorView {
    fn new(
        parent: &Entity<TerminalView>,
        (session_id, title, opened): (SessionId, String, Size<Pixels>),
        pin: Pin,
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
            content_view: None,
            _subscriptions: [repaint, close, resized],
            _pin_settle: pin_settle,
        }
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
            .child(div().flex_1().truncate().child(self.title.clone()))
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

    fn on_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(parent) = self.parent.upgrade() else { return };
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

    /// The grid for this frame: re-wrapped onto the mirror's own cells, or on
    /// the alternate screen the tab's grid with the font scaled to fit.
    fn grid(
        &mut self,
        frame: &PaneFrame,
        style: &MirrorStyle,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (width, height) = grid_extent(window);
        let (content, font) = if frame.alt_screen {
            let fitted = fit_font(&style.font, frame.dimensions, (width, height));
            (Arc::clone(&frame.content), fitted)
        } else {
            let grid = grid_cells(&style.font, width, height);
            (self.wrapped_content(frame, grid), style.font.clone())
        };
        let mut element = TerminalElement::new(
            content,
            font,
            style.colors.clone(),
            style.highlight,
            Rc::default(),
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
        let Some(frame) =
            source.shared.panes.lock().ok().and_then(|panes| panes.frame(self.session_id))
        else {
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
        let wanted =
            history_rows_wanted(grid_cells(&style.font, width, height), frame.dimensions.0);
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
    (usize::from(grid.cols), usize::from(grid.rows))
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
    let cursor_row = cursor.map(|cursor| cursor.point.row + history.len());

    // Logical lines, each with the cursor's offset when it sits on that line.
    let mut lines: Vec<(Vec<Cell>, Option<usize>)> = Vec::new();
    let mut line: Vec<Cell> = Vec::new();
    let mut line_cursor = None;
    for (row_index, row) in history.iter().chain(&content.rows).enumerate() {
        if let Some(cursor) = cursor.filter(|_| cursor_row == Some(row_index)) {
            line_cursor = Some(line.len() + cursor.point.col);
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

    let start = out.len().saturating_sub(rows);
    let shell_cursor = cursor.zip(out_cursor).and_then(|(cursor, (row, col))| {
        let row = row.checked_sub(start)?;
        Some(ShellCursor { point: ViewportPoint { row, col }, shape: cursor.shape })
    });
    let mut visible: Vec<Arc<Vec<Cell>>> = out.into_iter().skip(start).map(Arc::new).collect();
    let blank = Arc::new(vec![Cell::default(); cols]);
    visible.resize_with(rows, || Arc::clone(&blank));
    Content { rows: visible, display_offset: 0, pin_rows: 0, vi_cursor: None, shell_cursor }
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
}
