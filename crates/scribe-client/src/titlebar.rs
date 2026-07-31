//! Custom titlebar with an integrated tab bar for the GPUI client rebuild.
//!
//! The rebuild has a no-native-decorations mandate: the window is undecorated
//! and this [`TitlebarView`] draws the entire top chrome — a draggable move
//! region, the workspace-badge pill, the tab strip (active accent, per-tab close
//! button, drag-reorder slide, attention flash, AI activity dot, and the
//! context-% suffix), the equalize and gear icons, and the min/maximize/close
//! window controls. It is a `gpui::Entity` so its interactions mutate state and
//! emit [`TitlebarEvent`]s the shell reacts to; the pure layout/decay math lives
//! in [`crate::tab_bar`].

use gpui::{
    AnyElement, Context, ElementId, EventEmitter, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba, Role, Window,
    WindowControlArea, div, prelude::*, px,
};

use crate::tab_bar::{
    TabBarColors, TabData, flash_blend, px_units, reorder_target_index, tab_display_title,
};

/// Height of the titlebar in pixels.
pub const TITLEBAR_HEIGHT: f32 = 34.0;

/// Number of label columns a tab reserves at [`CHAR_WIDTH`] each.
const TAB_COLS: usize = 22;

/// Approximate advance width of one label character.
const CHAR_WIDTH: f32 = 8.0;

/// Fixed width of one tab in pixels (`TAB_COLS * CHAR_WIDTH`). Matching the
/// legacy fixed-column tab layout keeps the drag-reorder geometry deterministic.
pub const TAB_WIDTH: f32 = 176.0;

/// Pointer travel, in pixels, required before an armed press on the move region
/// hands the window to the compositor. Absorbs the jitter of an ordinary click
/// so a wobbly press on empty chrome does not turn into a window move.
const WINDOW_MOVE_THRESHOLD: f32 = 4.0;

/// A window-control button on the right edge of the titlebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowControlKind {
    Minimize,
    Maximize,
    Close,
}

/// Events the titlebar emits for the shell to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarEvent {
    /// A tab was clicked; make it active.
    SelectTab(usize),
    /// A tab's close button was clicked.
    CloseTab(usize),
    /// A tab finished a drag-reorder from `from` to `to`.
    ReorderTab { from: usize, to: usize },
    /// The gear icon was clicked (open settings).
    OpenSettings,
    /// The equalize icon was clicked (equalize the active tab's panes).
    Equalize,
    /// The workspace-notes affordance gained or lost pointer hover.
    WorkspaceNotesHover(bool),
    /// Open the workspace-notes modal.
    OpenWorkspaceNotes,
    /// A window-control button was clicked.
    WindowControl(WindowControlKind),
}

/// In-flight tab drag.
#[derive(Debug, Clone, Copy)]
struct DragState {
    /// Index of the tab being dragged (updated as it crosses neighbours).
    source: usize,
    /// Left edge of the first tab, in window pixels.
    origin_x: f32,
    /// Cursor X minus the dragged tab's left edge at grab time.
    grab_offset: f32,
    /// Current cursor X in window pixels.
    cursor_x: f32,
}

struct IconButton {
    id: ElementId,
    glyph: &'static str,
    label: &'static str,
    focus: FocusHandle,
    focused: bool,
}

struct TabClose<'a> {
    index: usize,
    id: ElementId,
    title: &'a str,
    foreground: Rgba,
    focused: bool,
}

struct TabChildren {
    display: String,
    ai_dot: Option<AnyElement>,
    suffix: Option<AnyElement>,
    close: Option<AnyElement>,
    underline: Option<AnyElement>,
}

#[derive(Clone, Copy)]
struct TabRender<'a> {
    index: usize,
    tab: &'a TabData,
    foreground: Rgba,
    id: &'a ElementId,
    focused: bool,
    close_focused: bool,
}

/// The custom titlebar view.
pub struct TitlebarView {
    colors: TabBarColors,
    tabs: Vec<TabData>,
    /// Workspace badge label + its accent color. `None` in single-workspace mode.
    badge: Option<(String, Rgba)>,
    show_gear: bool,
    show_equalize: bool,
    hovered_tab: Option<usize>,
    drag: Option<DragState>,
    /// Press origin recorded by a left press on the move region, or `None` when
    /// unarmed. Once the pointer travels [`WINDOW_MOVE_THRESHOLD`] px from it,
    /// the window is handed to the compositor via [`Window::start_window_move`].
    /// `WindowControlArea::Drag` is a no-op on Linux, so this is the real path.
    move_arm: Option<Point<Pixels>>,
    focus_handle: FocusHandle,
    tab_focus_handles: Vec<FocusHandle>,
    tab_close_focus_handles: Vec<FocusHandle>,
    equalize_focus_handle: FocusHandle,
    notes_focus_handle: FocusHandle,
    gear_focus_handle: FocusHandle,
    minimize_focus_handle: FocusHandle,
    maximize_focus_handle: FocusHandle,
    close_focus_handle: FocusHandle,
}

impl EventEmitter<TitlebarEvent> for TitlebarView {}

impl TitlebarView {
    /// Create a titlebar bound to the given chrome colors.
    pub fn new(colors: TabBarColors, cx: &mut Context<Self>) -> Self {
        Self {
            colors,
            tabs: Vec::new(),
            badge: None,
            show_gear: true,
            show_equalize: false,
            hovered_tab: None,
            drag: None,
            move_arm: None,
            focus_handle: cx.focus_handle(),
            tab_focus_handles: Vec::new(),
            tab_close_focus_handles: Vec::new(),
            equalize_focus_handle: cx.focus_handle(),
            notes_focus_handle: cx.focus_handle(),
            gear_focus_handle: cx.focus_handle(),
            minimize_focus_handle: cx.focus_handle(),
            maximize_focus_handle: cx.focus_handle(),
            close_focus_handle: cx.focus_handle(),
        }
    }

    /// Swap the chrome palette, e.g. after a live theme edit is hot-reloaded.
    pub fn set_colors(&mut self, colors: TabBarColors, cx: &mut Context<Self>) {
        self.colors = colors;
        cx.notify();
    }

    /// The chrome palette currently painted.
    pub const fn colors(&self) -> &TabBarColors {
        &self.colors
    }

    /// Replace the tab strip.
    pub fn set_tabs(&mut self, tabs: Vec<TabData>, cx: &mut Context<Self>) {
        self.tabs = tabs;
        while self.tab_focus_handles.len() < self.tabs.len() {
            self.tab_focus_handles.push(cx.focus_handle().tab_index(0));
            self.tab_close_focus_handles.push(cx.focus_handle().tab_index(0));
        }
        self.tab_focus_handles.truncate(self.tabs.len());
        self.tab_close_focus_handles.truncate(self.tabs.len());
        cx.notify();
    }

    /// Borrow the current tabs.
    pub fn tabs(&self) -> &[TabData] {
        &self.tabs
    }

    /// Set the workspace badge (label + accent), or clear it with `None`.
    pub fn set_badge(&mut self, badge: Option<(String, Rgba)>, cx: &mut Context<Self>) {
        self.badge = badge;
        cx.notify();
    }

    /// Toggle the equalize icon (shown only when the active tab has 2+ panes).
    pub fn set_show_equalize(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_equalize = show;
        cx.notify();
    }

    /// Make `index` the active tab and emit [`TitlebarEvent::SelectTab`].
    pub fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            tab.is_active = i == index;
        }
        cx.emit(TitlebarEvent::SelectTab(index));
        cx.notify();
    }

    /// Remove `index` and emit [`TitlebarEvent::CloseTab`], keeping one tab active.
    pub fn close(&mut self, index: usize, cx: &mut Context<Self>) {
        let was_active = match self.tabs.get(index) {
            Some(tab) => tab.is_active,
            None => return,
        };
        self.tabs.remove(index);
        self.tab_focus_handles.remove(index);
        self.tab_close_focus_handles.remove(index);
        if was_active && !self.tabs.is_empty() {
            let new_active = index.min(self.tabs.len() - 1);
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                tab.is_active = i == new_active;
            }
        }
        cx.emit(TitlebarEvent::CloseTab(index));
        cx.notify();
    }

    /// Begin dragging the tab at `source`; the grab offset is derived from the
    /// tab's laid-out left edge.
    pub fn begin_drag(
        &mut self,
        source: usize,
        cursor_x: f32,
        origin_x: f32,
        cx: &mut Context<Self>,
    ) {
        if source >= self.tabs.len() {
            return;
        }
        let tab_x = origin_x + px_units(source) * TAB_WIDTH;
        let grab_offset = cursor_x - tab_x;
        self.drag = Some(DragState { source, origin_x, grab_offset, cursor_x });
        cx.notify();
    }

    /// Update the active drag to `cursor_x`, reordering when the cursor crosses a
    /// neighbour. Emits [`TitlebarEvent::ReorderTab`] on each swap.
    pub fn update_drag(&mut self, cursor_x: f32, cx: &mut Context<Self>) {
        let Some(mut drag) = self.drag else { return };
        drag.cursor_x = cursor_x;
        let target =
            reorder_target_index(cursor_x, drag.origin_x, TAB_WIDTH, self.tabs.len(), drag.source);
        if target != drag.source {
            self.move_tab(drag.source, target);
            cx.emit(TitlebarEvent::ReorderTab { from: drag.source, to: target });
            drag.source = target;
        }
        self.drag = Some(drag);
        cx.notify();
    }

    /// End the active drag.
    pub fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Advance the window-move arm for one pointer motion.
    ///
    /// Returns `true` once the pointer has travelled [`WINDOW_MOVE_THRESHOLD`]
    /// px from the recorded press origin with the left button still held, which
    /// hands the window to the compositor and means the caller should skip the
    /// rest of its move handling.
    fn advance_move_arm(&mut self, event: &MouseMoveEvent, window: &Window) -> bool {
        if event.pressed_button != Some(MouseButton::Left) {
            // Motion with no button held means the press has ended, even if its
            // mouse-up landed outside the titlebar.
            self.move_arm = None;
            return false;
        }
        let Some(origin) = self.move_arm else {
            return false;
        };
        // Only travel past the jitter threshold counts as a drag; a press that
        // wobbles by a pixel stays an ordinary click.
        let travel =
            f32::from(event.position.x - origin.x).hypot(f32::from(event.position.y - origin.y));
        if travel < WINDOW_MOVE_THRESHOLD {
            return false;
        }
        self.move_arm = None;
        window.start_window_move();
        true
    }

    /// Move a tab from `from` to `to`, shifting the tabs in between.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        let focus = self.tab_focus_handles.remove(from);
        self.tab_focus_handles.insert(to, focus);
        let close_focus = self.tab_close_focus_handles.remove(from);
        self.tab_close_focus_handles.insert(to, close_focus);
    }

    /// Left edge of the first tab, in pixels: after the badge pill.
    fn tabs_origin_x(&self) -> f32 {
        self.badge.as_ref().map_or(0.0, |(label, _)| badge_width_px(label))
    }

    /// Whether a titlebar control owns keyboard focus. The shell uses this to
    /// avoid taking focus back to the terminal during an ordinary repaint.
    pub fn has_keyboard_focus(&self, window: &Window) -> bool {
        self.tab_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.tab_close_focus_handles.iter().any(|handle| handle.is_focused(window))
            || self.equalize_focus_handle.is_focused(window)
            || self.notes_focus_handle.is_focused(window)
            || self.gear_focus_handle.is_focused(window)
            || self.minimize_focus_handle.is_focused(window)
            || self.maximize_focus_handle.is_focused(window)
            || self.close_focus_handle.is_focused(window)
    }

    fn focus_next_or_previous(
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.key != "tab" {
            return false;
        }
        if event.keystroke.modifiers.shift {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        true
    }

    fn tab_key(
        &mut self,
        index: usize,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if Self::focus_next_or_previous(event, window, cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if modifiers.control && modifiers.shift && matches!(key, "arrowleft" | "arrowright") {
            let to = if key == "arrowleft" {
                index.saturating_sub(1)
            } else {
                (index + 1).min(self.tabs.len().saturating_sub(1))
            };
            if to != index {
                self.move_tab(index, to);
                cx.emit(TitlebarEvent::ReorderTab { from: index, to });
                self.focus_tab(to, window, cx);
                cx.notify();
            }
            return;
        }
        if matches!(key, "arrowleft" | "arrowright")
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            let to = if key == "arrowleft" {
                index.saturating_sub(1)
            } else {
                (index + 1).min(self.tabs.len().saturating_sub(1))
            };
            if to != index {
                self.focus_tab(to, window, cx);
            }
            return;
        }
        if matches!(key, "enter" | "space") {
            self.select(index, cx);
        }
    }

    fn focus_after_close(&self, closed: usize, window: &mut Window, cx: &mut Context<Self>) {
        let next = closed.min(self.tabs.len().saturating_sub(1));
        if let Some(focus) = self.tab_focus_handles.get(next) {
            window.focus(focus, cx);
        }
    }

    fn focus_tab(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = self.tab_focus_handles.get(index) {
            window.focus(focus, cx);
        }
    }
}

/// Pixel width of the badge pill for `label` (leading + trailing pad + gap).
#[must_use]
pub fn badge_width_px(label: &str) -> f32 {
    px_units(label.chars().count() + 2) * CHAR_WIDTH + 16.0
}

/// Build the semi-transparent per-pane title pill element.
///
/// Rendered by the shell over the top-right of a split pane's content; exposed
/// here so the titlebar owns all task-title chrome. Truncates with an ellipsis
/// past `max_cols` columns.
pub fn pane_title_pill(title: &str, colors: &TabBarColors, max_cols: usize) -> AnyElement {
    let (display, _truncated) = tab_display_title(title, max_cols.max(1));
    let bg = Rgba { r: colors.bg.r, g: colors.bg.g, b: colors.bg.b, a: 0.7 };
    div()
        .px_2()
        .py_0p5()
        .bg(bg)
        .text_color(colors.text)
        .text_xs()
        .rounded_sm()
        .child(display)
        .into_any_element()
}

impl TitlebarView {
    fn render_badge(&self) -> Option<AnyElement> {
        let (label, accent) = self.badge.as_ref()?;
        let pill_bg = Rgba { r: accent.r, g: accent.g, b: accent.b, a: 0.25 };
        Some(
            div()
                .flex()
                .items_center()
                .h_full()
                .px_2()
                .mr_1()
                .bg(pill_bg)
                .rounded_sm()
                .text_color(self.colors.active_text)
                .text_xs()
                .child(label.clone())
                .into_any_element(),
        )
    }

    /// Base tab background before the attention flash is blended in.
    fn tab_base_bg(&self, tab: &TabData, is_hovered: bool) -> Rgba {
        if tab.is_active {
            self.colors.active_bg
        } else if is_hovered {
            let bg = self.colors.bg;
            Rgba {
                r: (bg.r + 0.04).min(1.0),
                g: (bg.g + 0.04).min(1.0),
                b: (bg.b + 0.04).min(1.0),
                a: bg.a,
            }
        } else {
            self.colors.gradient_top
        }
    }

    /// Drag slide offset for the dragged tab (it follows the cursor).
    fn tab_slide_offset(&self, index: usize) -> Option<f32> {
        self.drag.and_then(|d| {
            (d.source == index).then(|| {
                let tab_x = self.tabs_origin_x() + px_units(index) * TAB_WIDTH;
                d.cursor_x - d.grab_offset - tab_x
            })
        })
    }

    fn render_tab_close(&self, close: TabClose<'_>, cx: &mut Context<Self>) -> AnyElement {
        let TabClose { index, id: tab_id, title, foreground: fg, focused } = close;
        let Some(focus) = self.tab_close_focus_handles.get(index).cloned() else {
            return div().into_any_element();
        };
        div()
            .id((tab_id, "close"))
            .role(Role::Button)
            .aria_label(format!("Close {title}"))
            .track_focus(&focus)
            .ml_1()
            .px_0p5()
            .text_color(fg)
            .when(focused, |this| this.bg(self.colors.accent).text_color(self.colors.active_text))
            .child("\u{00D7}")
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, ctx| {
                window.focus(&focus, ctx);
                this.close(index, ctx);
                this.focus_after_close(index, window, ctx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, ctx| {
                ctx.stop_propagation();
                if Self::focus_next_or_previous(event, window, ctx) {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.close(index, ctx);
                    this.focus_after_close(index, window, ctx);
                }
            }))
            .into_any_element()
    }

    fn render_tab_children(&self, render: TabRender<'_>, cx: &mut Context<Self>) -> TabChildren {
        let TabRender { index, tab, foreground, id, focused, close_focused } = render;
        let suffix_len = tab.context_suffix.as_ref().map_or(0, |s| s.text.chars().count());
        let available = TAB_COLS.saturating_sub(4).saturating_sub(suffix_len);
        let (display, _truncated) = tab_display_title(&tab.title, available);
        let ai_dot = tab
            .ai_indicator
            .map(|color| div().size(px(6.0)).rounded_full().bg(color).mr_1().into_any_element());
        let suffix = tab.context_suffix.as_ref().map(|suffix| {
            div().text_color(suffix.color).child(suffix.text.clone()).into_any_element()
        });
        let close = (tab.is_active || self.hovered_tab == Some(index) || focused).then(|| {
            self.render_tab_close(
                TabClose {
                    index,
                    id: id.clone(),
                    title: &tab.title,
                    foreground,
                    focused: close_focused,
                },
                cx,
            )
        });
        let underline = tab.is_active.then(|| {
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(2.0))
                .bg(self.colors.accent)
                .into_any_element()
        });
        TabChildren { display, ai_dot, suffix, close, underline }
    }

    fn render_tab(
        &self,
        index: usize,
        tab: &TabData,
        focused_window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_hovered = self.hovered_tab == Some(index);
        let base_bg = self.tab_base_bg(tab, is_hovered);
        let bg = flash_blend(base_bg, self.colors.accent, tab.tab_flash);
        let fg = if tab.is_active { self.colors.active_text } else { self.colors.text };
        let tab_id = ElementId::from(tab.accessibility_id.clone());
        let Some(tab_focus) = self.tab_focus_handles.get(index).cloned() else {
            return div().into_any_element();
        };
        let focused = tab_focus.is_focused(focused_window);
        let close_focused = self
            .tab_close_focus_handles
            .get(index)
            .is_some_and(|focus| focus.is_focused(focused_window));
        let children = self.render_tab_children(
            TabRender { index, tab, foreground: fg, id: &tab_id, focused, close_focused },
            cx,
        );

        let mut tab_el = div()
            .id(tab_id)
            .role(Role::Tab)
            .aria_label(tab.title.clone())
            .aria_selected(tab.is_active)
            .aria_position_in_set(index + 1)
            .aria_size_of_set(self.tabs.len())
            .track_focus(&tab_focus)
            .relative()
            .flex()
            .items_center()
            .flex_none()
            .w(px(TAB_WIDTH))
            .h_full()
            .px_2()
            .bg(bg)
            .text_color(fg)
            .text_sm()
            .border_r_1()
            .border_color(self.colors.separator);
        if focused {
            tab_el = tab_el.border_2().border_color(self.colors.accent);
        }
        if let Some(dx) = self.tab_slide_offset(index) {
            tab_el = tab_el.left(px(dx));
        }

        tab_el
            .children(children.ai_dot)
            .child(div().flex_1().overflow_hidden().child(children.display))
            .children(children.suffix)
            .children(children.close)
            .children(children.underline)
            .on_hover(cx.listener(move |this, hovered: &bool, _win, ctx| {
                this.hovered_tab = if *hovered { Some(index) } else { None };
                ctx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _win, ctx| {
                    let cursor_x = f32::from(event.position.x);
                    let origin_x = this.tabs_origin_x();
                    this.begin_drag(index, cursor_x, origin_x, ctx);
                }),
            )
            .on_click(cx.listener(move |this, _, window, ctx| {
                window.focus(&tab_focus, ctx);
                if this.drag.is_none() {
                    this.select(index, ctx);
                }
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, ctx| {
                ctx.stop_propagation();
                this.tab_key(index, event, window, ctx);
            }))
            .into_any_element()
    }

    fn render_icon_button(
        &self,
        button: IconButton,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Context<Self>) + Clone + 'static,
    ) -> AnyElement {
        let hover_bg = self.colors.gradient_top;
        let key_activate = on_click.clone();
        let IconButton { id, glyph, label, focus, focused } = button;
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label)
            .track_focus(&focus)
            .flex()
            .items_center()
            .justify_center()
            .w(px(34.0))
            .h_full()
            .text_color(self.colors.text)
            .hover(move |s| s.bg(hover_bg))
            .when(focused, |this| this.bg(self.colors.accent).text_color(self.colors.active_text))
            .child(glyph)
            // Swallow the press so pointer jitter cannot arm a window move.
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(cx.listener(move |_, _, win, ctx| {
                win.focus(&focus, ctx);
                on_click(ctx);
            }))
            .on_key_down(cx.listener(move |_, event: &KeyDownEvent, win, ctx| {
                ctx.stop_propagation();
                if Self::focus_next_or_previous(event, win, ctx) {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    key_activate(ctx);
                }
            }))
            .into_any_element()
    }

    fn render_workspace_notes_button(
        &self,
        focused_window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover_bg = self.colors.gradient_top;
        let focus = self.notes_focus_handle.clone();
        div()
            .id("workspace-notes")
            .role(Role::Button)
            .aria_label("Show workspace notes")
            .track_focus(&focus)
            .flex()
            .items_center()
            .justify_center()
            .w(px(34.0))
            .h_full()
            .text_color(self.colors.text)
            .hover(move |style| style.bg(hover_bg))
            .when(focus.is_focused(focused_window), |this| {
                this.bg(self.colors.accent).text_color(self.colors.active_text)
            })
            .child("N")
            .on_hover(cx.listener(|_, hovered: &bool, _win, ctx| {
                ctx.emit(TitlebarEvent::WorkspaceNotesHover(*hovered));
            }))
            .on_mouse_move(cx.listener(|_, _: &MouseMoveEvent, _win, ctx| {
                ctx.emit(TitlebarEvent::WorkspaceNotesHover(true));
            }))
            // Swallow the press so pointer jitter cannot arm a window move.
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(cx.listener(move |_, _, win, ctx| {
                win.focus(&focus, ctx);
                ctx.emit(TitlebarEvent::OpenWorkspaceNotes);
            }))
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, win, ctx| {
                ctx.stop_propagation();
                if Self::focus_next_or_previous(event, win, ctx) {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    ctx.emit(TitlebarEvent::OpenWorkspaceNotes);
                }
            }))
            .into_any_element()
    }

    fn render_window_control(
        &self,
        kind: WindowControlKind,
        focused_window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, glyph, label, area, hover_bg) = match kind {
            WindowControlKind::Minimize => (
                "wc-min",
                "\u{2013}",
                "Minimize window",
                WindowControlArea::Min,
                self.colors.gradient_top,
            ),
            WindowControlKind::Maximize => (
                "wc-max",
                "\u{25A1}",
                "Maximize window",
                WindowControlArea::Max,
                self.colors.gradient_top,
            ),
            // Close hovers red for the destructive affordance.
            WindowControlKind::Close => (
                "wc-close",
                "\u{00D7}",
                "Close window",
                WindowControlArea::Close,
                Rgba { r: 0.784, g: 0.188, b: 0.188, a: 1.0 },
            ),
        };
        let focus = match kind {
            WindowControlKind::Minimize => self.minimize_focus_handle.clone(),
            WindowControlKind::Maximize => self.maximize_focus_handle.clone(),
            WindowControlKind::Close => self.close_focus_handle.clone(),
        };
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label)
            .track_focus(&focus)
            .flex()
            .items_center()
            .justify_center()
            .w(px(40.0))
            .h_full()
            .text_color(self.colors.text)
            .hover(move |s| s.bg(hover_bg))
            .when(focus.is_focused(focused_window), |this| {
                this.bg(self.colors.accent).text_color(self.colors.active_text)
            })
            .window_control_area(area)
            .child(glyph)
            // Swallow the press so pointer jitter cannot arm a window move.
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(cx.listener(move |_, _, win, ctx| {
                win.focus(&focus, ctx);
                Self::activate_window_control(kind, win, ctx);
            }))
            .on_key_down(cx.listener(move |_, event: &KeyDownEvent, win, ctx| {
                ctx.stop_propagation();
                if Self::focus_next_or_previous(event, win, ctx) {
                    return;
                }
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    Self::activate_window_control(kind, win, ctx);
                }
            }))
            .into_any_element()
    }

    fn activate_window_control(
        kind: WindowControlKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match kind {
            WindowControlKind::Minimize => window.minimize_window(),
            WindowControlKind::Maximize => window.zoom_window(),
            WindowControlKind::Close => window.remove_window(),
        }
        cx.emit(TitlebarEvent::WindowControl(kind));
    }

    fn render_tabs(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.tabs
            .clone()
            .iter()
            .enumerate()
            .map(|(index, tab)| self.render_tab(index, tab, window, cx))
            .collect()
    }

    fn render_equalize_button(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.show_equalize.then(|| {
            self.render_icon_button(
                IconButton {
                    id: ElementId::from("equalize"),
                    glyph: "\u{229E}",
                    label: "Equalize panes",
                    focus: self.equalize_focus_handle.clone(),
                    focused: self.equalize_focus_handle.is_focused(window),
                },
                cx,
                |ctx| ctx.emit(TitlebarEvent::Equalize),
            )
        })
    }

    fn render_gear_button(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.show_gear.then(|| {
            self.render_icon_button(
                IconButton {
                    id: ElementId::from("gear"),
                    glyph: "\u{2699}",
                    label: "Open settings",
                    focus: self.gear_focus_handle.clone(),
                    focused: self.gear_focus_handle.is_focused(window),
                },
                cx,
                |ctx| ctx.emit(TitlebarEvent::OpenSettings),
            )
        })
    }
}

impl Render for TitlebarView {
    fn render(
        &mut self,
        focused_window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tabs = self.render_tabs(focused_window, cx);
        let equalize = self.render_equalize_button(focused_window, cx);
        let gear = self.render_gear_button(focused_window, cx);
        let workspace_notes = self.render_workspace_notes_button(focused_window, cx);

        div()
            .track_focus(&self.focus_handle)
            .id("titlebar")
            .role(Role::TitleBar)
            .aria_label("Scribe title bar")
            .flex()
            .items_center()
            .w_full()
            // Fixed-height band above the flex-grown terminal grid; see
            // [`crate::window_chrome`] for why none of the bands may shrink.
            .flex_none()
            .h(px(TITLEBAR_HEIGHT))
            .bg(self.colors.bg)
            .border_b_1()
            .border_color(self.colors.separator)
            // Declared for Windows, where the platform consults the hit-test
            // areas. X11/Wayland ignore it, so the handlers below drive the move.
            .window_control_area(WindowControlArea::Drag)
            // Bubble-phase listeners run front-to-back, so a tab's own
            // `on_mouse_down` has already armed `drag` by the time this runs;
            // an active tab drag therefore never arms a window move.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _win, _ctx| {
                    this.move_arm = this.drag.is_none().then_some(event.position);
                }),
            )
            // A press anywhere outside the titlebar ends any arm it left behind.
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _win, _ctx| {
                this.move_arm = None;
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, win, ctx| {
                if this.advance_move_arm(event, win) {
                    return;
                }
                if this.drag.is_some() && event.pressed_button == Some(MouseButton::Left) {
                    this.update_drag(f32::from(event.position.x), ctx);
                }
                let width = f32::from(win.bounds().size.width);
                let x = f32::from(event.position.x);
                if (width - 188.0..width - 154.0).contains(&x) {
                    ctx.emit(TitlebarEvent::WorkspaceNotesHover(true));
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _win, ctx| {
                    this.move_arm = None;
                    this.end_drag(ctx);
                }),
            )
            .children(self.render_badge())
            .child(
                div()
                    .id("terminal-tabs")
                    .role(Role::TabList)
                    .aria_label("Terminal tabs")
                    .flex()
                    .items_center()
                    .h_full()
                    .flex_none()
                    .children(tabs),
            )
            // Draggable spacer fills the gap between tabs and the right controls.
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .children(equalize)
            .child(workspace_notes)
            .children(gear)
            .child(self.render_window_control(WindowControlKind::Minimize, focused_window, cx))
            .child(self.render_window_control(WindowControlKind::Maximize, focused_window, cx))
            .child(self.render_window_control(WindowControlKind::Close, focused_window, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex},
    };

    use gpui::{AppContext as _, Entity, TestAppContext};
    use scribe_common::theme::minimal_dark;

    use super::{TitlebarEvent, TitlebarView};
    use crate::tab_bar::{TabBarColors, TabData};

    /// Create a titlebar seeded with `n` tabs (the first active) and a captured
    /// event log.
    fn titlebar_with_tabs(
        n: usize,
        cx: &mut TestAppContext,
    ) -> (Entity<TitlebarView>, Arc<Mutex<Vec<TitlebarEvent>>>) {
        let colors = TabBarColors::from(&minimal_dark().chrome);
        let bar = cx.new(|cx| {
            let mut bar = TitlebarView::new(colors, cx);
            let tabs = (0..n)
                .map(|i| {
                    let mut tab = TabData::new(format!("tab-{i}"));
                    tab.is_active = i == 0;
                    tab
                })
                .collect();
            bar.set_tabs(tabs, cx);
            bar
        });
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let record = move |event: &TitlebarEvent| {
            if let Ok(mut guard) = sink.lock() {
                guard.push(*event);
            }
        };
        cx.update(|app| {
            app.subscribe(&bar, move |_, event: &TitlebarEvent, _| record(event)).detach();
        });
        cx.update(|_| {});
        (bar, log)
    }

    // @lat: [[client#GPUI Titlebar#Selecting a tab activates it and emits]]
    #[gpui::test]
    fn select_activates_the_tab_and_emits(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| bar.select(2, cx));
        bar.read_with(cx, |bar, _| {
            assert!(bar.tabs()[2].is_active);
            assert!(!bar.tabs()[0].is_active);
        });
        assert_eq!(log.lock().unwrap().as_slice(), &[TitlebarEvent::SelectTab(2)]);
    }

    // @lat: [[client#GPUI Titlebar#Closing a tab removes it and reactivates]]
    #[gpui::test]
    fn close_removes_and_keeps_one_active(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        bar.update(cx, |bar, cx| bar.close(0, cx));
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs().len(), 2);
            assert!(bar.tabs().iter().filter(|t| t.is_active).count() == 1);
        });
        assert_eq!(log.lock().unwrap().as_slice(), &[TitlebarEvent::CloseTab(0)]);
    }

    // @lat: [[client#GPUI Titlebar#Drag reorder moves the tab and emits]]
    #[gpui::test]
    fn drag_reorders_the_tab_and_emits(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(3, cx);
        // Grab tab 0 at its left edge and drag past tab 2.
        bar.update(cx, |bar, cx| {
            bar.begin_drag(0, 0.0, 0.0, cx);
            bar.update_drag(super::TAB_WIDTH * 2.5, cx);
            bar.end_drag(cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs()[2].title, "tab-0");
        });
        assert!(log.lock().unwrap().iter().any(|e| matches!(e, TitlebarEvent::ReorderTab { .. })));
    }

    // @lat: [[client#GPUI Titlebar#Out-of-range interactions are no-ops]]
    #[gpui::test]
    fn out_of_range_interactions_are_noops(cx: &mut TestAppContext) {
        let (bar, log) = titlebar_with_tabs(2, cx);
        bar.update(cx, |bar, cx| {
            bar.select(9, cx);
            bar.close(9, cx);
            bar.begin_drag(9, 0.0, 0.0, cx);
        });
        bar.read_with(cx, |bar, _| assert_eq!(bar.tabs().len(), 2));
        assert!(log.lock().unwrap().is_empty());
    }

    // @lat: [[client#GPUI Titlebar#Accessibility IDs survive tab reordering]]
    #[gpui::test]
    fn accessibility_ids_are_unique_and_follow_reordered_tabs(cx: &mut TestAppContext) {
        let (bar, _) = titlebar_with_tabs(3, cx);
        let original_ids = bar.read_with(cx, |bar, _| {
            bar.tabs().iter().map(|tab| tab.accessibility_id.clone()).collect::<Vec<_>>()
        });
        assert_eq!(original_ids.iter().collect::<HashSet<_>>().len(), original_ids.len());

        bar.update(cx, |bar, cx| {
            bar.begin_drag(0, 0.0, 0.0, cx);
            bar.update_drag(super::TAB_WIDTH * 2.5, cx);
            bar.end_drag(cx);
        });
        bar.read_with(cx, |bar, _| {
            assert_eq!(bar.tabs()[2].accessibility_id, original_ids[0]);
        });
    }
}
