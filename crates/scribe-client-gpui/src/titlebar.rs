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
    AnyElement, Context, ElementId, EventEmitter, FocusHandle, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Rgba, WindowControlArea, div, prelude::*, px,
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
    focus_handle: FocusHandle,
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
            focus_handle: cx.focus_handle(),
        }
    }

    /// Replace the tab strip.
    pub fn set_tabs(&mut self, tabs: Vec<TabData>, cx: &mut Context<Self>) {
        self.tabs = tabs;
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

    /// Move a tab from `from` to `to`, shifting the tabs in between.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
    }

    /// Left edge of the first tab, in pixels: after the badge pill.
    fn tabs_origin_x(&self) -> f32 {
        self.badge.as_ref().map_or(0.0, |(label, _)| badge_width_px(label))
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

    fn render_tab_close(index: usize, fg: Rgba, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(("tab-close", index))
            .ml_1()
            .px_0p5()
            .text_color(fg)
            .child("\u{00D7}")
            .on_mouse_down(MouseButton::Left, |_, _win, ctx| ctx.stop_propagation())
            .on_click(cx.listener(move |this, _, _win, ctx| this.close(index, ctx)))
            .into_any_element()
    }

    fn render_tab(&self, index: usize, tab: &TabData, cx: &mut Context<Self>) -> AnyElement {
        let is_hovered = self.hovered_tab == Some(index);
        let base_bg = self.tab_base_bg(tab, is_hovered);
        let bg = flash_blend(base_bg, self.colors.accent, tab.tab_flash);
        let fg = if tab.is_active { self.colors.active_text } else { self.colors.text };

        let suffix_len = tab.context_suffix.as_ref().map_or(0, |s| s.text.chars().count());
        let available = TAB_COLS.saturating_sub(4).saturating_sub(suffix_len);
        let (display, _truncated) = tab_display_title(&tab.title, available);

        let ai_dot =
            tab.ai_indicator.map(|color| div().size(px(6.0)).rounded_full().bg(color).mr_1());
        let suffix =
            tab.context_suffix.as_ref().map(|s| div().text_color(s.color).child(s.text.clone()));
        let close = (tab.is_active || is_hovered).then(|| Self::render_tab_close(index, fg, cx));
        let underline = tab.is_active.then(|| {
            div().absolute().bottom_0().left_0().right_0().h(px(2.0)).bg(self.colors.accent)
        });

        let mut tab_el = div()
            .id(("tab", index))
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
        if let Some(dx) = self.tab_slide_offset(index) {
            tab_el = tab_el.left(px(dx));
        }

        tab_el
            .children(ai_dot)
            .child(div().flex_1().overflow_hidden().child(display))
            .children(suffix)
            .children(close)
            .children(underline)
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
            .on_click(cx.listener(move |this, _, _win, ctx| {
                if this.drag.is_none() {
                    this.select(index, ctx);
                }
            }))
            .into_any_element()
    }

    fn render_icon_button(
        &self,
        id: ElementId,
        glyph: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Context<Self>) + 'static,
    ) -> AnyElement {
        let hover_bg = self.colors.gradient_top;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(34.0))
            .h_full()
            .text_color(self.colors.text)
            .hover(move |s| s.bg(hover_bg))
            .child(glyph)
            .on_click(cx.listener(move |_, _, _win, ctx| on_click(ctx)))
            .into_any_element()
    }

    fn render_window_control(&self, kind: WindowControlKind, cx: &mut Context<Self>) -> AnyElement {
        let (id, glyph, area, hover_bg) = match kind {
            WindowControlKind::Minimize => {
                ("wc-min", "\u{2013}", WindowControlArea::Min, self.colors.gradient_top)
            }
            WindowControlKind::Maximize => {
                ("wc-max", "\u{25A1}", WindowControlArea::Max, self.colors.gradient_top)
            }
            // Close hovers red for the destructive affordance.
            WindowControlKind::Close => (
                "wc-close",
                "\u{00D7}",
                WindowControlArea::Close,
                Rgba { r: 0.784, g: 0.188, b: 0.188, a: 1.0 },
            ),
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(40.0))
            .h_full()
            .text_color(self.colors.text)
            .hover(move |s| s.bg(hover_bg))
            .window_control_area(area)
            .child(glyph)
            .on_click(cx.listener(move |_, _, win, ctx| {
                match kind {
                    WindowControlKind::Minimize => win.minimize_window(),
                    WindowControlKind::Maximize => win.zoom_window(),
                    WindowControlKind::Close => win.remove_window(),
                }
                ctx.emit(TitlebarEvent::WindowControl(kind));
            }))
            .into_any_element()
    }
}

impl Render for TitlebarView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_data = self.tabs.clone();
        let mut tabs = Vec::with_capacity(tab_data.len());
        for (i, tab) in tab_data.iter().enumerate() {
            tabs.push(self.render_tab(i, tab, cx));
        }

        let equalize = self.show_equalize.then(|| {
            self.render_icon_button(ElementId::from("equalize"), "\u{229E}", cx, |ctx| {
                ctx.emit(TitlebarEvent::Equalize);
            })
        });
        let gear = self.show_gear.then(|| {
            self.render_icon_button(ElementId::from("gear"), "\u{2699}", cx, |ctx| {
                ctx.emit(TitlebarEvent::OpenSettings);
            })
        });

        div()
            .track_focus(&self.focus_handle)
            .id("titlebar")
            .flex()
            .items_center()
            .w_full()
            .h(px(TITLEBAR_HEIGHT))
            .bg(self.colors.bg)
            .border_b_1()
            .border_color(self.colors.separator)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _win, ctx| {
                if this.drag.is_some() && event.pressed_button == Some(MouseButton::Left) {
                    this.update_drag(f32::from(event.position.x), ctx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _win, ctx| this.end_drag(ctx)),
            )
            .children(self.render_badge())
            .child(div().flex().items_center().h_full().flex_none().children(tabs))
            // Draggable spacer fills the gap between tabs and the right controls.
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .children(equalize)
            .children(gear)
            .child(self.render_window_control(WindowControlKind::Minimize, cx))
            .child(self.render_window_control(WindowControlKind::Maximize, cx))
            .child(self.render_window_control(WindowControlKind::Close, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

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
}
